---
layout: home

hero:
  name: SnipDesk
  text: Fast snippet launcher for support agents.
  tagline: >-
    Hit a global hotkey, type a few characters, press Enter. The canned reply
    pastes into whatever window you were just in.
  actions:
    - theme: brand
      text: Install (5 min)
      link: /getting-started
    - theme: alt
      text: Self-host the server
      link: /docker-quickstart
    - theme: alt
      text: GitHub
      link: https://github.com/2lukewil/snipdesk

features:
  - title: Lite (free, offline)
    details: >-
      Global hotkey, fuzzy search, variables, folders, tags, auto-paste,
      import and export. Snippets live on the device. No account required.
    link: /getting-started
    linkText: Install and first run
  - title: Teams (server-backed)
    details: >-
      Sync personal snippets across devices from the desktop app or the
      browser extension, share a team library, sign in with passwords or
      SSO (Google Workspace, Keycloak), manage users from a browser
      dashboard. Personal snippets are encrypted on the server.
    link: /docker-quickstart
    linkText: Spin up a server
  - title: Self-hosted and whitelabel
    details: >-
      One Rust binary backed by a single SQLite file. Runs in Docker
      anywhere from a Raspberry Pi to a Kubernetes cluster. Rebuild
      the client and server with your own branding without forking.
    link: /whitelabel
    linkText: Build a branded image
---

## What's in this site

- **[Getting started](/getting-started)** - install the desktop client, learn the hotkey, add your first snippet, sign in to Teams if your org runs a server.
- **[Browser extension](/extension)** - the same launcher and library as a Chrome extension for pasting into web apps (WHMCS, mail, chat): offline-first, SSO, and fleet-installable.
- **[Docker quickstart](/docker-quickstart)** - fresh-machine-to-working-dashboard in about five minutes for the self-hosted server.
- **[Production deploy](/deploy)** - TLS, reverse proxy, OIDC (Google Workspace + Keycloak / any compliant IdP), backups, retention, security checklist.
- **[Server architecture](/server-design)** - schema, sync algorithm, encryption posture, JWT + refresh-token rotation.
- **[Build from source](/build)** - prerequisites, dev loops for both editions, local server, edition flags, the docs site itself.
- **[Auto-update & releases](/auto-update)** - how tagged pushes build, sign, and publish installers; one-time signing-key setup.
- **[Whitelabel brand bundles](/whitelabel)** - rebuild the client and server for a customer-branded image without touching the upstream repo.

::: tip Looking for the source?
Every page on this site is a markdown file under
[`docs/`](https://github.com/2lukewil/snipdesk/tree/main/docs) in the repo,
so you can read the same content directly on GitHub or send a PR with the
"Edit this page" link at the bottom of any page.
:::
