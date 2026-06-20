# SnipDesk browser extension

A Chrome (Manifest V3) client for the SnipDesk server: a snippet
launcher and team library that runs in the browser. It talks to the
same self-hosted server as the desktop client and shares no code with
it; the only dependency is the server's HTTP API.

## Layout

- `src/background/` - service worker: owns the JWT, makes all server
  API calls, runs the sync loop, routes the launcher command.
- `src/content/` - content script + in-page overlay launcher. Captures
  the focused field, renders search, inserts the chosen snippet.
- `src/manager/` - full-page manager: sign-in, library browse,
  personal snippet CRUD, settings, savings.
- `src/popup/` - toolbar popup: status and quick links.
- `src/shared/` - API client, validation limits, and ported pure logic
  (search, variables, savings). No imports from the desktop client.

## Develop

```
npm install
npm run build      # outputs dist/
```

Then load `dist/` as an unpacked extension: `chrome://extensions` ->
Developer mode -> Load unpacked -> select `dist/`.

`npm run dev` runs Vite with HMR for iterative work.

## Package

```
npm run build
npm run zip        # -> snipdesk-extension.zip
```

## Server requirement

The server needs the extension's auth-redirect URL in its OIDC
allowlist for one-click SSO. See `docs/extension.md` for the setting
and for fleet deployment via Chrome enterprise policy.
