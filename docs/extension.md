# Browser extension

The SnipDesk browser extension brings the snippet launcher and personal
library to Chrome (and Chromium-based browsers). Personal snippets work
entirely on-device with no account; signing in to a SnipDesk server is
optional and adds the shared team library and cross-device sync. It runs
on every OS with no installer and inserts snippets into the web fields
agents actually work in.

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

Signing in is **optional** - personal snippets work without it. Sign in
to add the team library and sync across devices. Open the toolbar popup
and enter your server URL (or it's prefilled when an admin pinned it; see
[Pin the server URL](#pin-the-server-url-no-rebuild)). The popup then
offers whatever the server allows:

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
manifest (see [Self-host the extension](#self-host-the-extension-private-crx)
below). The manifest pins a stable extension ID via its `key`, so the
ID (and therefore the SSO redirect URL) is consistent across installs.

### Self-host the extension (private CRX)

No Web Store listing required: pack a signed `.crx`, host it next to an
update manifest, and force-install it by URL. All you need is a web
server the managed browsers can reach over HTTPS (an internal host on
the VPN is fine, since the devices are managed).

#### 1. Build and pack

Build first, then pack the `extension/dist` output (not the source):

```bash
cd extension && npm install && npm run build
```

Pack with Chrome's built-in packer (or the **Pack extension** button on
`chrome://extensions`):

```
chrome.exe --pack-extension="C:\path\to\extension\dist" --pack-extension-key="C:\path\to\snipdesk.pem"
```

This writes `dist.crx` (the package you host). On the first run, omit
`--pack-extension-key` and Chrome generates a fresh `dist.pem` beside the
folder. **The `.pem` signs every future update: back it up and never
commit it.** Losing it means you can't ship updates under the same ID.

#### Extension ID and the signing key

Chrome derives the extension ID from the package's public key. The repo
commits a `key` in the manifest so every build already reports the
production ID `pmbbmppiinigigajakmffkchlibmdebo` (see
`extension/manifest.config.js`); the matching private key is
deliberately not committed.

- **You have that private key:** pack with it. The CRX is signed by the
  same keypair the manifest `key` pins, so the ID stays
  `pmbbmppiinigigajakmffkchlibmdebo` and nothing on the server changes.
- **You don't:** packing generates a new keypair, which yields a
  different ID. Replace the `key` in `extension/manifest.config.js` with
  your new public key so source builds and the hosted CRX agree on the
  ID, rebuild, then update that new ID in two places:
  `SNIPDESK_OIDC_EXTENSION_REDIRECTS` on the server (see
  [SSO redirect allowlist](#sso-redirect-allowlist)) and the
  managed-storage policy key path (see
  [Pin the server URL](#pin-the-server-url-no-rebuild)).

#### 2. Write the update manifest

Chrome polls an Omaha-protocol XML manifest to discover the current
version. Host it next to the `.crx`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<gupdate xmlns="http://www.google.com/update2/response" protocol="2.0">
  <app appid="pmbbmppiinigigajakmffkchlibmdebo">
    <updatecheck codebase="https://snippets.example.com/ext/snipdesk.crx" version="1.0.0" />
  </app>
</gupdate>
```

- `appid` is the extension ID.
- `codebase` is the HTTPS URL of the `.crx`.
- `version` must match the version in the packed manifest (it comes from
  `extension/package.json`). Bump it on every update.

#### 3. Host over HTTPS

Serve `updates.xml` and the `.crx` from any HTTPS endpoint the managed
browsers can reach: an internal web server, object storage, or the
GitLab generic package registry. HTTPS is required; the host can be
internal/VPN-only since the target devices are managed.

#### 4. Force-install through Google Workspace

Admin console -> Devices -> Chrome -> Apps & extensions -> **Users &
browsers**, pick the org unit, then add the extension:

1. Click the **+**, choose **Add Chrome app or extension by ID**.
2. Enter the ID `pmbbmppiinigigajakmffkchlibmdebo`, set the installation
   source to **From a custom URL**, and give it your update manifest URL
   (`https://snippets.example.com/ext/updates.xml`).
3. Set **Installation policy** to **Force install**.

The raw-policy equivalent (Windows Group Policy without Workspace) is an
`ExtensionInstallForcelist` entry of the form `<id>;<update_url>`:

```
pmbbmppiinigigajakmffkchlibmdebo;https://snippets.example.com/ext/updates.xml
```

Pair either with the pinned `server_url` policy below so the
force-installed extension arrives already pointed at your server.

#### Pushing an update

Bump the version in `extension/package.json`, rebuild, and pack a new
`.crx` with the same `.pem`. Upload it, then update `version` (and
`codebase` if the filename changed) in `updates.xml`. Managed browsers
pick it up on their next update poll, typically within a few hours.

### Pin the server URL (no rebuild)

The extension reads an admin-managed `server_url` from Chrome's managed
storage, so a sysadmin can bake in the SnipDesk server at deploy time the
same way the desktop build's locked URL works, without repackaging the
extension. When the policy is set, the extension adopts that URL and the
popup's server field is locked, so agents only choose a sign-in method.

The default-built extension ID is `pmbbmppiinigigajakmffkchlibmdebo` (a
Web Store listing reassigns its own ID; use whatever ID the install
reports on `chrome://extensions`).

**Google Workspace Admin console** (easiest): Devices -> Chrome -> Apps &
extensions -> the SnipDesk extension -> "Policy for extensions", paste:

```json
{ "server_url": { "Value": "https://snippets.example.com" } }
```

**Windows Group Policy / registry**: set a string value `server_url`
under the extension's policy key:

```
HKLM\Software\Policies\Google\Chrome\3rdparty\extensions\pmbbmppiinigigajakmffkchlibmdebo\policy
  server_url = https://snippets.example.com
```

**macOS / Linux** use the same managed-policy mechanism via a
configuration profile / managed-policy JSON keyed by the extension ID.

Pair this with the `ExtensionInstallForcelist` entry above and agents get
a force-installed extension already pointed at your server. Changing the
policy value rolls the whole fleet on Chrome's next policy refresh.

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
