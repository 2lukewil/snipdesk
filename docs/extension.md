# Browser extension

The SnipDesk browser extension brings the snippet launcher and personal
library to Chrome (and Chromium-based browsers). It talks to the same
server as the desktop client, runs on every OS with no installer, and
inserts snippets into the web fields agents actually work in.

## Install

### From a packed build (recommended for testing)

1. Build it: `cd extension && npm install && npm run build`. The
   unpacked extension lands in `extension/dist`.
2. Open `chrome://extensions`, enable **Developer mode**.
3. Click **Load unpacked** and choose `extension/dist`.

`npm run zip` produces a distributable `.zip` of the same output.

### Keyboard shortcut

The launcher opens with **Ctrl+Shift+Space** (**Command+Shift+Space** on
macOS). Rebind it at `chrome://extensions/shortcuts`, or from the
manager's **Settings -> Keyboard shortcut -> Change shortcut**.

## Sign in

Open the toolbar popup and enter your server URL. The popup then offers
whatever the server allows:

- **Password** sign-in, when the server has it enabled.
- **One-click SSO** buttons, one per configured OIDC provider. These use
  Chrome's `launchWebAuthFlow`; see [SSO redirect](#sso-redirect-allowlist).
- **Paste token** fallback, under the disclosure, for any flow that ends
  with a token.

The extension stores the JWT in `chrome.storage.local`, syncs every five
minutes, and refreshes the token automatically as it nears expiry.

## Using it

- **Launcher** (hotkey, or **Launch here** in the popup): search by
  title, narrow by tag with a `#tag` token, fill `{variables}`, and
  insert into the focused field. Inline autocomplete completes the top
  match; Tab or Right accepts it.
- **Manager** (toolbar **Open manager**): browse and edit personal
  snippets, filter by tag, multi-select for bulk move/delete, organise
  folders (create, rename, nest, reorder, and delete with a move-to-Unfiled
  or delete-contents choice), import/export with a folder-grouped
  selection tree, search and restore from Trash, and configure savings,
  theme, density, sort, and usage counts.
- **Add selection to SnipDesk**: right-click selected text on any page to
  start a new snippet prefilled with it.

Team-library snippets appear read-only with a cloud marker; they are
managed in the dashboard (the admin-only **Team library** link opens it).

## Offline and sync

Personal snippets are offline-first. Searching, inserting, creating,
editing, deleting, folder operations, and Trash all work with the server
unreachable: changes apply to a local cache immediately and queue in an
outbox that flushes on the next sync (every five minutes, on a manual
**Sync now**, or at sign-in). The popup and the manager header show how
many changes are still pending, and flag a failed sync.

Cross-device conflicts resolve **last-write-wins** (your local edit wins
on reconcile). The team library is read-only and online only. Sync uses
`GET /api/snippets?since=N` deltas with soft-delete tombstones; client
IDs are minted locally so offline-created snippets upload cleanly.

## Fleet deployment

Distribute the extension to a team with Chrome's
[`ExtensionInstallForcelist`](https://chromeenterprise.google/policies/#ExtensionInstallForcelist)
policy, pointing at the Web Store listing or a self-hosted update
manifest. The manifest pins a stable extension ID via its `key`, so the
ID (and therefore the SSO redirect URL) is consistent across installs.

## SSO redirect allowlist

One-click SSO sends the browser to
`https://<extension-id>.chromiumapp.org/` after authentication. The
server only redirects to URLs it has been told to trust, so add that URL
to the OIDC extension-redirect allowlist:

```
SNIPDESK_OIDC_EXTENSION_REDIRECTS=https://<extension-id>.chromiumapp.org/
```

Multiple URLs are comma-separated. The default-built extension ID is
`pmbbmppiinigigajakmffkchlibmdebo`; a Web Store listing reassigns its own
ID, at which point this value is updated to match. No CORS configuration
is needed: the background worker fetches with host permissions, which are
not subject to page CORS.

## Notes and limitations

- **Empty folders and manual folder order are stored per browser
  profile.** The server has no personal-folders table, so an empty folder
  (or a custom order) lives in `chrome.storage` until a snippet is filed
  into it, after which it syncs like any other folder.
- **Canvas-based editors** (such as Google Docs) expose no standard DOM
  field and are not supported. Standard inputs, textareas, and
  contenteditable regions work, including ones inside a same-origin
  iframe. A contenteditable inside a **cross-origin** iframe can't be
  reached from the page's top frame (browser security), so insertion
  there isn't supported.
- The launcher follows the Dark/Light theme setting like the manager and
  popup.

## Develop

From `extension/`: `npm run dev` for a watch build, `npm run build` for a
production build, `npm run zip` to package, and `npm test` to run the unit
tests (the pure search/variable/validation logic, via Node's built-in
runner). CI runs the tests and build on every change.
