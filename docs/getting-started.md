# Getting started with SnipDesk

A short walkthrough for first-time users. Covers the launcher
basics, then the optional Teams sign-in if your team runs a
snipdesk-server.

## What SnipDesk is

A keyboard-driven snippet launcher. You press a global hotkey
(`Alt+Space` by default), type a few characters, hit Enter, and the
snippet you wanted gets pasted into whatever window you were just
in - a ticket reply, a Slack message, an email draft.

It runs in the system tray. You don't keep the window open; you
summon it when you need it, dismiss it when you're done.

## Installing

Download the latest installer from the [Releases
page](https://github.com/2lukewil/snipdesk/releases). On Windows,
that's a `.msi` or `.exe`; on macOS, a `.dmg`; on Linux,
`.deb`/`.rpm`/`.AppImage`.

Install it like any other app. SnipDesk installs per-user (no admin
prompt on Windows). The window doesn't open after install - the app
is now sitting in the system tray, waiting for the hotkey.

If you don't see the tray icon, look in the overflow area (the small
arrow next to the clock on Windows). You can right-click the icon to
open the main window, change the hotkey, or quit.

## The first run

Press `Alt+Space` anywhere. The launcher window appears, focused on
the search box.

It's empty - you haven't added any snippets yet. Three things to
try, in order:

### 1. Add your first snippet

`Ctrl+N` (or click the **New** button).

Fill in:

- **Title**: a short label you'll recognise in search. Think
  "Refund acknowledgment", not "RA-2024-Q4-v2".
- **Body**: the actual text you want to paste. Multi-line is fine.
- **Folder**: optional. A `/`-separated path like `Billing/Refunds`
  if you want to organise. Folders are created on demand.
- **Tags**: optional, comma-separated. Used by the filter strip and
  search.

Save (`Ctrl+S` or the **Save** button).

### 2. Paste it somewhere

Open whatever app you usually paste into - email, ticket reply,
Slack. Click into the text field so the cursor's there.

Press `Alt+Space`. Type the first few characters of your snippet's
title. The list filters as you type. Hit `Enter`.

SnipDesk hides itself, returns focus to your previous window, and
pastes the snippet.

That's the entire core loop.

### 3. Try variables

`{placeholders}` in a snippet body become fill-in prompts at paste
time. Useful for canned replies that need a customer name, an
invoice ID, or any per-paste detail.

```
Hi {customer_name},

Your refund for invoice #{invoice_id} has been processed. It
should show up on {payment_method} within 3-5 business days.

Best,
Support
```

On paste, SnipDesk prompts for each variable; values from previous
uses are offered as suggestions. Names can use letters, digits,
`_`, and `-`.

## Keyboard cheat sheet

| Key | Action |
| --- | --- |
| `Alt+Space` | Toggle launcher (global; works from any app) |
| Type in search | Filter by title / body / tags |
| `↑` / `↓` | Navigate the list |
| `Enter` | Paste selected snippet |
| `Shift+Enter` | Copy to clipboard only (no auto-paste) |
| `Ctrl+N` | New snippet |
| `Ctrl+E` | Edit selected |
| `Delete` | Delete selected (with confirmation) |
| `Ctrl+,` | Open Settings |
| `Esc` | Clear search/filter; second press hides the window |

The launcher reopens to whatever folder + filter + search you had
last time, so the second press of `Alt+Space` in a row picks up where
you left off.

## Folders + drag-and-drop

The left sidebar shows your folders. Click a folder to filter the
list to its contents. Click **All snippets** to clear the filter.

You can drag snippets between folders to reorganise. You can also
drag a folder onto another folder to nest it. Right-click a folder
for rename / delete / merge.

The **Unfiled** entry collects snippets that aren't in any folder.

## Settings worth knowing about

`Ctrl+,` opens the settings modal. The tabs to look at:

- **General**: paste behaviour (auto-paste vs clipboard-only), the
  hotkey, "start with Windows", "minimize to tray on close".
- **Appearance**: dark / light / system theme, accent colour,
  compact density.
- **Editor**: customise the formatting toolbar (the Bold / Italic /
  Code / Link buttons above the snippet body). Each rule is a
  `prefix` / `suffix` pair; the default rules ship Markdown-style
  syntax. Add your own if your ticketing tool uses BBCode or
  something else.
- **About**: app version, log file location, manual "Check for
  updates" button. Logs live next to your snippet database (see
  Data location below).

## Auto-paste vs clipboard-only

Two paste modes:

- **Auto-paste** (default): SnipDesk hides itself, returns focus to
  the window you were in before opening the launcher, and
  synthesises `Ctrl+V` for you. One keystroke from "press hotkey" to
  "snippet is pasted".
- **Clipboard only**: SnipDesk just copies the snippet to your
  clipboard. You paste manually with `Ctrl+V` (or
  `Cmd+V` / middle-click / whatever you usually do).

Auto-paste is faster but requires the synthesised keystroke to land
on the right window. If you're working with a screen reader, a
remote-desktop session that swallows keystrokes, or in Wayland
(where keystroke synthesis is unreliable), set the mode to
"Clipboard only" in Settings -> General.

## Data location

Your snippets and settings live in the OS app-data folder, under the
build's identifier (`com.snipdesk.lite` for the free edition,
`com.snipdesk.teams` for the Teams edition):

- **Windows**: `%APPDATA%\com.snipdesk.lite\`
- **macOS**: `~/Library/Application Support/com.snipdesk.lite/`
- **Linux**: `~/.local/share/com.snipdesk.lite/`

In that folder:

- `snippets.db` - your SQLite snippet database. The whole point.
- `settings.json` - app settings.
- `backups/` - rolling daily snapshots of `snippets.db`. Configurable
  retention in Settings -> About.
- `snipdesk.log` - structured log file. Useful when reporting bugs.

To move to a new machine: copy the whole folder. Or use Settings ->
Import/Export -> Export to dump JSON, then Import on the new machine.

## Teams sign-in (optional)

If your team runs a snipdesk-server, you can sync personal snippets
across devices and get access to the team-curated shared library.

If you're using SnipDesk for yourself (no server), skip this section
entirely - the free edition is full-featured offline.

1. Get the server URL from your admin (something like
   `https://snippets.yourcompany.com`).
2. Open Settings -> Server.
3. Enter the server URL.
4. Pick one (the sign-in options the server shows match what your
   admin has configured):
   - **Sign in with Google** or **Sign in with <your org's SSO>** -
     opens your browser, you sign in with your work account, return
     to SnipDesk. The button label reflects whichever identity
     provider your admin set up (Google Workspace, Keycloak, Okta,
     etc.).
   - **Email + password** - fill in the boxes, click **Sign in** or
     **Create account**. (Whoever signs up first becomes the org's
     admin; usually that's not you.)
5. If you have existing local snippets, you'll be asked whether to
   upload them. Say yes for first-time setup so they're available on
   your other devices too.

Once signed in:

- Your snippets sync automatically every 60 minutes (or click **Sync
  now** in Settings to trigger immediately). The interval is
  configurable per device in Settings.
- Shared library snippets from your team appear in the launcher
  with a small cloud glyph. They're mixed into the main view by
  default; toggle them off in Settings -> Server if you
  prefer them only in the dedicated **Team Library** folder.
- A **Trash** folder appears in the sidebar - deleted snippets stay
  there for 90 days (or whatever your admin set), recoverable with
  one click.

## Multi-device usage

Sign in on each device with the same account. Snippets sync within
a few minutes; offline edits queue locally and push when you
reconnect.

If you edit the same snippet on two devices while both are offline,
the second to sync wins. In practice this is extremely rare:
synced devices only conflict if both go offline, both edit the same
snippet, then both come back online.

## When something doesn't work

- **The hotkey doesn't open the launcher.** Another app probably
  claimed `Alt+Space` (browser zoom controls; some screen sharing
  apps). Change the hotkey in Settings -> General. `Ctrl+Shift+S`
  and `Win+/` are reliable alternatives.
- **The window appears but the paste lands somewhere wrong.** The
  app remembered the wrong "previous window" because of how the
  hotkey fired. Try setting paste mode to "Clipboard only" in
  Settings -> General as a workaround; or report a bug with your OS
  + which apps you have open.
- **Snippets aren't syncing.** Check Settings -> Server that
  the server URL is set and `signed in as <you>` appears. Click
  **Sync now** to force. If it errors, the error message says why
  (network unreachable, server expired your session, etc.).
- **Sign-in with Google opens the browser but doesn't come back.**
  The browser landing page should auto-redirect to SnipDesk via a
  `snipdesk://` link. If the OS didn't claim the link, the page
  also shows your sign-in token in a copy-paste field - copy it
  and paste into the **Paste sign-in token** field in Settings ->
  Server.

For anything else, the `snipdesk.log` file in your data folder is
the right thing to attach to a bug report.

## What's next

- Settings -> Editor lets you customise the formatting toolbar. If
  your ticketing tool uses BBCode or RAW HTML, swap the defaults.
- Settings -> Appearance has a compact density mode that fits more
  snippets on screen.

The rest is discoverable from the in-app menus.
