// Service worker: the only place that holds the JWT and talks to the
// server. UI surfaces (content overlay, popup, manager) send MSG.*
// messages and get a { ok, data } / { ok:false, error } envelope back.

import { MSG } from "../shared/messages.js";
import * as api from "../shared/api.js";
import * as store from "../shared/storage.js";

const SYNC_ALARM = "snipdesk-sync";
const SYNC_PERIOD_MIN = 5;

const CTX_ADD_SNIPPET = "snipdesk-add-selection";

chrome.runtime.onInstalled.addListener((details) => {
  chrome.alarms.create(SYNC_ALARM, { periodInMinutes: SYNC_PERIOD_MIN });
  setupContextMenu();
  adoptManagedServerUrl();
  // Fresh install: open the manager, which runs the first-run tour.
  if (details.reason === "install") {
    chrome.tabs.create({ url: chrome.runtime.getURL("src/manager/index.html") });
  }
});
chrome.runtime.onStartup.addListener(() => {
  chrome.alarms.create(SYNC_ALARM, { periodInMinutes: SYNC_PERIOD_MIN });
  setupContextMenu();
  adoptManagedServerUrl();
});

// A policy-pinned server URL is authoritative: adopt it into settings so
// every sign-in/sync path uses it. Mirrors the desktop's locked-URL
// behavior. Re-checked each startup so a policy change rolls the fleet.
async function adoptManagedServerUrl() {
  const { server_url: url } = await store.getManaged();
  const pinned = (url || "").trim();
  if (pinned && (await store.getServerUrl()) !== pinned) {
    await store.setSettings({ server_url: pinned });
  }
}
chrome.alarms.onAlarm.addListener((a) => {
  if (a.name === SYNC_ALARM) syncNow().catch(() => {});
});

chrome.commands.onCommand.addListener((command) => {
  if (command === "open-launcher") {
    launcherOnActiveTab();
    // Tell any open extension page (the onboarding) the shortcut fired.
    // Best-effort: no receiver when nothing's listening.
    chrome.runtime.sendMessage({ type: MSG.LAUNCHER_OPENED }).catch(() => {});
  }
});

function launcherOnActiveTab() {
  chrome.tabs.query({ active: true, currentWindow: true }, (tabs) => {
    const id = tabs[0]?.id;
    if (id != null) {
      chrome.tabs.sendMessage(id, { type: MSG.TOGGLE_LAUNCHER }).catch(() => {});
    }
  });
}

// Right-click selected text on any page to start a new snippet from it.
function setupContextMenu() {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: CTX_ADD_SNIPPET,
      title: "Add selection to SnipDesk",
      contexts: ["selection"],
    });
  });
}
chrome.contextMenus?.onClicked.addListener((info) => {
  if (info.menuItemId === CTX_ADD_SNIPPET && info.selectionText) {
    store.setPendingNewSnippet(info.selectionText).then(() => {
      chrome.runtime.openOptionsPage();
    });
  }
});

// ---- auth helpers ----

const trimUrl = (u) => (u || "").trim().replace(/\/+$/, "");

function launchWebAuthFlow(url) {
  return new Promise((resolve, reject) => {
    chrome.identity.launchWebAuthFlow({ url, interactive: true }, (redirectUrl) => {
      if (chrome.runtime.lastError) reject(new Error(chrome.runtime.lastError.message));
      else if (!redirectUrl) reject(new Error("sign-in was cancelled"));
      else resolve(redirectUrl);
    });
  });
}

async function authError(e) {
  if (e instanceof api.ApiError && (e.kind === "unauthorized" || e.kind === "inactive")) {
    await store.clearSession();
    broadcastAuthChanged();
    return { ok: false, error: e.message, signedOut: true, reason: e.kind };
  }
  return { ok: false, error: e?.message || String(e) };
}

// Tell open extension pages (popup, manager) that auth state changed so
// they live-update. Best-effort: no receiver when nothing's listening.
function broadcastAuthChanged() {
  chrome.runtime.sendMessage({ type: MSG.AUTH_CHANGED }).catch(() => {});
}

async function afterSignIn(serverUrl, auth) {
  await store.setToken(auth.token);
  await store.setUser(auth.user);
  await store.setSettings({ server_url: serverUrl });
  broadcastAuthChanged();
  await syncNow();
  return { ok: true, data: { user: auth.user } };
}

// ---- sync ----

async function pullStream(kind, fetcher) {
  const cache = await store.getCache(kind);
  const res = await fetcher(cache.hwm || 0);
  for (const view of res.snippets || []) {
    const prev = cache.items[view.id];
    // Never overwrite an item with un-flushed local edits; the outbox
    // owns it until it reconciles (last-write-wins).
    if (prev && (prev.dirty || prev.pendingCreate || prev.needsRestore)) continue;
    if (view.is_deleted) {
      // Keep a tombstone locally (with whatever payload we already had)
      // so Trash works offline; drop content-less ones we never saw.
      if (prev) {
        prev.deleted = true;
        prev.syncedTombstone = true;
        prev.version = view.version;
        prev.deletedAt = prev.deletedAt || view.updated_at || Math.floor(Date.now() / 1000);
      }
    } else if (view.payload) {
      cache.items[view.id] = {
        id: view.id,
        version: view.version,
        updated_at: view.updated_at,
        ...view.payload,
        uses: prev?.uses || 0,
        last_used: prev?.last_used || null,
      };
    }
  }
  cache.hwm = res.high_water_mark ?? cache.hwm;
  await store.setCache(kind, cache);
}

// ---- offline write queue (outbox) ----
// Personal-snippet writes apply to the cache immediately and carry
// flags: pendingCreate (never reached the server), dirty (edited since
// last sync), deleted (tombstoned locally). flushOutbox pushes them up.
let flushing = false;
let flushQueued = false;

function flushSoon() {
  flushOutbox().catch(() => {});
}

async function serverVersionFor(serverUrl, token, id) {
  try {
    const res = await api.listSnippets(serverUrl, token, 0);
    const hit = (res.snippets || []).find((x) => x.id === id && !x.is_deleted);
    return hit ? hit.version : null;
  } catch {
    return null;
  }
}

const payloadOf = (it) => ({
  title: it.title || "",
  body: it.body || "",
  tags: it.tags || [],
  folder_path: it.folder_path || null,
});

async function flushOutbox() {
  if (flushing) {
    flushQueued = true;
    return;
  }
  flushing = true;
  try {
    const serverUrl = await store.getServerUrl();
    const token = await store.getToken();
    if (!serverUrl || !token) return;
    const cache = await store.getCache("personal");
    let changed = false;
    let broke = false;
    let breakErr = null;
    for (const id of Object.keys(cache.items)) {
      const it = cache.items[id];
      if (!it.dirty && !it.pendingCreate && !it.needsRestore) continue;
      try {
        if (it.deleted) {
          // A queued delete. Items that never reached the server settle as
          // a local-only tombstone (kept so Trash works offline); synced
          // ones get deleted server-side and become a synced tombstone.
          if (it.pendingCreate) {
            it.pendingCreate = false;
            it.dirty = false;
            it.localOnly = true;
          } else {
            await api.deleteSnippet(serverUrl, token, id);
            it.dirty = false;
            it.syncedTombstone = true;
          }
        } else if (it.needsRestore) {
          await api.restoreTrash(serverUrl, token, id);
          it.needsRestore = false;
          it.syncedTombstone = false;
          it.dirty = false;
        } else if (it.pendingCreate) {
          const res = await api.createSnippet(serverUrl, token, id, payloadOf(it));
          it.version = res.version;
          it.pendingCreate = false;
          it.dirty = false;
        } else {
          const res = await api.updateSnippet(serverUrl, token, id, it.version, payloadOf(it));
          it.version = res.version;
          it.dirty = false;
        }
        changed = true;
      } catch (e) {
        if (e instanceof api.ApiError && e.kind === "network") {
          broke = true;
          breakErr = "server unreachable; changes will retry";
          break; // server down: stay queued
        }
        if (e instanceof api.ApiError && (e.kind === "unauthorized" || e.kind === "inactive")) {
          broke = true;
          breakErr = e.message;
          break;
        }
        if (e instanceof api.ApiError && e.status === 409 && e.code === "id_taken" && it.pendingCreate) {
          // The server already has this id (created on another device
          // before this create flushed). Reconcile by updating in place
          // instead of looping on a create that can't succeed.
          const v = await serverVersionFor(serverUrl, token, id);
          if (v != null) {
            try {
              const res = await api.updateSnippet(serverUrl, token, id, v, payloadOf(it));
              it.version = res.version;
              it.pendingCreate = false;
              it.dirty = false;
              changed = true;
            } catch {
              /* leave pendingCreate; next flush retries */
            }
          }
          continue;
        }
        if (e instanceof api.ApiError && e.kind === "conflict" && !it.deleted && !it.pendingCreate) {
          // Last-write-wins: refetch the current version, overwrite.
          const v = await serverVersionFor(serverUrl, token, id);
          if (v != null) {
            try {
              const res = await api.updateSnippet(serverUrl, token, id, v, payloadOf(it));
              it.version = res.version;
              it.dirty = false;
              changed = true;
              continue;
            } catch {
              /* leave dirty; next flush retries */
            }
          }
        } else if (it.deleted) {
          // Already gone server-side: settle as a synced tombstone.
          it.dirty = false;
          it.syncedTombstone = true;
          changed = true;
        } else if (it.needsRestore) {
          it.needsRestore = false; // give up (avoid a retry loop)
          changed = true;
        }
      }
    }
    if (changed) await store.setCache("personal", cache);
    // Surface flush outcome so the popup/manager show "couldn't sync"
    // even when the flush was triggered by a write rather than a sync.
    if (broke) await store.setSyncStatus({ at: Date.now(), ok: false, error: breakErr });
    else if (changed) await store.setSyncStatus({ at: Date.now(), ok: true });
  } finally {
    flushing = false;
    if (flushQueued) {
      flushQueued = false;
      flushOutbox();
    }
  }
}

async function pendingCount() {
  const cache = await store.getCache("personal");
  return Object.values(cache.items).filter((it) => it.dirty || it.pendingCreate || it.needsRestore).length;
}

// Drop fully-synced tombstones once they age out, matching the server's
// retention, so local trash doesn't grow without bound. Un-flushed
// deletes (still dirty) are always kept.
const TOMBSTONE_TTL_DAYS = 90;
async function pruneTombstones() {
  const cache = await store.getCache("personal");
  const cutoff = Math.floor(Date.now() / 1000) - TOMBSTONE_TTL_DAYS * 86400;
  let changed = false;
  for (const id of Object.keys(cache.items)) {
    const it = cache.items[id];
    if (it.deleted && (it.syncedTombstone || it.localOnly) && !it.dirty && it.deletedAt && it.deletedAt < cutoff) {
      delete cache.items[id];
      changed = true;
    }
  }
  if (changed) await store.setCache("personal", cache);
}

async function syncNow() {
  const serverUrl = await store.getServerUrl();
  let token = await store.getToken();
  if (!serverUrl || !token) return { ok: false, error: "not signed in" };
  try {
    const meRes = await api.me(serverUrl, token);
    if (meRes?.refreshed_token) {
      await store.setToken(meRes.refreshed_token);
      token = meRes.refreshed_token;
    }
    if (meRes?.user) await store.setUser(meRes.user);
    await flushOutbox(); // push local writes up before pulling
    await pullStream("personal", (since) => api.listSnippets(serverUrl, token, since));
    await pullStream("library", (since) => api.listLibrary(serverUrl, token, since));
    await pruneTombstones();
    await store.setSyncStatus({ at: Date.now(), ok: true });
    return { ok: true };
  } catch (e) {
    await store.setSyncStatus({ at: Date.now(), ok: false, error: e?.message || String(e) });
    return authError(e);
  }
}

// Merged, non-deleted view for the overlay/manager.
async function snippetsForUi() {
  const personal = await store.getCache("personal");
  const library = await store.getCache("library");
  const map = (cache, source) =>
    Object.values(cache.items)
      .filter((it) => !it.deleted) // hide locally-tombstoned items
      .map((it) => ({ ...it, source }));
  return [...map(personal, "personal"), ...map(library, "library")];
}

// ---- message router ----

const handlers = {
  [MSG.PING]: async () => ({ ok: true }),

  [MSG.AUTH_STATUS]: async () => {
    const serverUrl = await store.getServerUrl();
    const token = await store.getToken();
    const user = await store.getUser();
    const lastSync = await store.getSyncStatus();
    const pending = await pendingCount();
    return { ok: true, data: { signedIn: Boolean(token && user), user, serverUrl, lastSync, pending } };
  },

  [MSG.LAUNCH_HERE]: async () => {
    launcherOnActiveTab();
    return { ok: true };
  },

  [MSG.AUTH_METHODS]: async ({ serverUrl }) => {
    try {
      return { ok: true, data: await api.authMethods(serverUrl) };
    } catch (e) {
      return { ok: false, error: e.message };
    }
  },

  [MSG.AUTH_LOGIN]: async ({ serverUrl, email, password }) => {
    try {
      const auth = await api.login(serverUrl, email, password);
      return await afterSignIn(serverUrl, auth);
    } catch (e) {
      return { ok: false, error: e.message };
    }
  },

  [MSG.AUTH_SIGNUP]: async ({ serverUrl, email, password, displayName }) => {
    try {
      const auth = await api.signup(serverUrl, email, password, displayName);
      return await afterSignIn(serverUrl, auth);
    } catch (e) {
      return { ok: false, error: e.message };
    }
  },

  // Validate a pasted token via /api/me before trusting it.
  [MSG.AUTH_PASTE_TOKEN]: async ({ serverUrl, token }) => {
    try {
      const meRes = await api.me(serverUrl, token);
      await store.setToken(token);
      await store.setUser(meRes.user);
      await store.setSettings({ server_url: serverUrl });
      broadcastAuthChanged();
      await syncNow();
      return { ok: true, data: { user: meRes.user } };
    } catch (e) {
      return { ok: false, error: e.message };
    }
  },

  // One-click SSO: open the server's OIDC start in launchWebAuthFlow
  // with our chromiumapp.org redirect; the server 302s back to it
  // with the token in the query, which we validate via /api/me.
  [MSG.AUTH_SSO]: async ({ serverUrl, startUrl }) => {
    try {
      const redirectUri = chrome.identity.getRedirectURL();
      const authUrl =
        `${trimUrl(serverUrl)}${startUrl}?redirect=${encodeURIComponent(redirectUri)}`;
      const finalUrl = await launchWebAuthFlow(authUrl);
      const token = new URL(finalUrl).searchParams.get("token");
      if (!token) return { ok: false, error: "no token returned from sign-in" };
      // Validate the token inline so a bad one still fails fast, but defer
      // the first sync: reporting success only after a full personal +
      // library pull is what made sign-in feel slow. The popup flips to
      // signed-in immediately and snippets fill in as the sync lands; the
      // periodic sync reconciles if this kick gets cut short.
      const meRes = await api.me(serverUrl, token);
      await store.setToken(token);
      await store.setUser(meRes.user);
      await store.setSettings({ server_url: serverUrl });
      broadcastAuthChanged();
      syncNow().catch(() => {});
      return { ok: true, data: { user: meRes.user } };
    } catch (e) {
      return { ok: false, error: e.message };
    }
  },

  [MSG.AUTH_LOGOUT]: async () => {
    await store.clearSession();
    broadcastAuthChanged();
    return { ok: true };
  },

  [MSG.SETTINGS_GET]: async () => ({ ok: true, data: await store.getSettings() }),
  [MSG.SETTINGS_SET]: async ({ patch }) => ({
    ok: true,
    data: await store.setSettings(patch),
  }),

  [MSG.SYNC_NOW]: async () => syncNow(),
  [MSG.SNIPPETS_GET]: async () => ({ ok: true, data: await snippetsForUi() }),

  // Writes apply to the local cache immediately and queue for the
  // outbox; flushSoon attempts to push to the server but failure (e.g.
  // offline) just leaves the item queued for the next sync.
  [MSG.SNIPPET_CREATE]: async ({ id, payload }) => {
    const cache = await store.getCache("personal");
    // Honour a caller-supplied id (import preserving an exported id) so a
    // re-import is idempotent; mint one otherwise. If the id already
    // exists locally, fall back to a fresh one rather than clobber it.
    const newId = id && !cache.items[id] ? id : crypto.randomUUID();
    cache.items[newId] = { id: newId, version: 0, ...payload, uses: 0, last_used: null, dirty: true, pendingCreate: true };
    await store.setCache("personal", cache);
    flushSoon();
    return { ok: true, data: { id: newId, version: 0 } };
  },

  [MSG.SNIPPET_UPDATE]: async ({ id, payload }) => {
    const cache = await store.getCache("personal");
    const it = cache.items[id];
    if (!it) return { ok: false, error: "snippet not found" };
    Object.assign(it, payload, { dirty: true });
    await store.setCache("personal", cache);
    flushSoon();
    return { ok: true, data: { version: it.version } };
  },

  [MSG.SNIPPET_DELETE]: async ({ id }) => {
    const cache = await store.getCache("personal");
    const it = cache.items[id];
    if (it) {
      it.deleted = true;
      it.deletedAt = Math.floor(Date.now() / 1000);
      if (it.pendingCreate) {
        // Never reached the server: keep a local-only tombstone so Trash
        // works offline and the delete stays reversible. Nothing to sync.
        it.pendingCreate = false;
        it.dirty = false;
        it.localOnly = true;
      } else {
        it.dirty = true;
      }
      await store.setCache("personal", cache);
      flushSoon();
    }
    return { ok: true };
  },

  // Remove exact-content duplicates: group live snippets by title+body,
  // keep the most-used in each group, tombstone the rest (settles like
  // any other delete, so it syncs and the trash stays consistent).
  [MSG.SNIPPET_DEDUPE]: async () => {
    const cache = await store.getCache("personal");
    const groups = new Map();
    for (const it of Object.values(cache.items)) {
      if (it.deleted) continue;
      const key = `${it.title || ""} ${it.body || ""}`;
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(it);
    }
    let removed = 0;
    for (const group of groups.values()) {
      if (group.length < 2) continue;
      group.sort((a, b) => (b.uses || 0) - (a.uses || 0));
      for (const it of group.slice(1)) {
        it.deleted = true;
        it.deletedAt = Math.floor(Date.now() / 1000);
        if (it.pendingCreate) {
          it.pendingCreate = false;
          it.dirty = false;
          it.localOnly = true;
        } else {
          it.dirty = true;
        }
        removed++;
      }
    }
    if (removed) {
      await store.setCache("personal", cache);
      flushSoon();
    }
    return { ok: true, data: { removed } };
  },

  // Trash is served from local tombstones (works offline), merged with
  // the server's trash for items deleted on other devices when online.
  [MSG.TRASH_LIST]: async ({ localOnly } = {}) => {
    const cache = await store.getCache("personal");
    const local = Object.values(cache.items)
      .filter((it) => it.deleted)
      .map((it) => ({
        id: it.id,
        version: it.version,
        deleted_at: it.deletedAt || null,
        payload: payloadOf(it),
      }));
    const byDeleted = (a, b) => (b.deleted_at || 0) - (a.deleted_at || 0);
    const serverUrl = await store.getServerUrl();
    const token = await store.getToken();
    // Local tombstones render instantly. Skip the server round-trip when
    // the caller only wants the local set, or when we're plainly offline,
    // so Trash never blocks on an unreachable host.
    if (localOnly || !serverUrl || !token || !navigator.onLine) {
      return { ok: true, data: local.sort(byDeleted), partial: !localOnly };
    }
    const localIds = new Set(local.map((x) => x.id));
    let merged = local;
    try {
      const remote = (await api.listTrash(serverUrl, token)) || [];
      merged = [...local, ...remote.filter((r) => !localIds.has(r.id))];
    } catch {
      /* offline or error: local-only trash is still useful */
    }
    return { ok: true, data: merged.sort(byDeleted) };
  },

  [MSG.TRASH_RESTORE]: async ({ id }) => {
    const cache = await store.getCache("personal");
    const it = cache.items[id];
    if (it && it.deleted) {
      // Local tombstone: undelete optimistically. A never-synced item is
      // restored by re-queuing its create; a synced one queues a restore.
      it.deleted = false;
      delete it.deletedAt;
      if (it.localOnly) {
        it.localOnly = false;
        it.pendingCreate = true;
        it.dirty = true;
      } else {
        it.dirty = true;
        if (it.syncedTombstone) it.needsRestore = true;
      }
      await store.setCache("personal", cache);
      flushSoon();
      return { ok: true };
    }
    // Server-only tombstone (deleted on another device): needs the server.
    const serverUrl = await store.getServerUrl();
    const token = await store.getToken();
    try {
      await api.restoreTrash(serverUrl, token, id);
      await syncNow();
      return { ok: true };
    } catch (e) {
      return authError(e);
    }
  },

  [MSG.VAR_HISTORY_GET]: async () => ({ ok: true, data: await store.getVarHistory() }),
  [MSG.VAR_HISTORY_ADD]: async ({ snippetId, values }) => {
    const hist = await store.getVarHistory();
    const key = snippetId;
    hist[key] = hist[key] || {};
    for (const [name, value] of Object.entries(values || {})) {
      if (!value) continue;
      const list = hist[key][name] || [];
      const next = [value, ...list.filter((v) => v !== value)].slice(0, 8);
      hist[key][name] = next;
    }
    await store.setVarHistory(hist);
    return { ok: true };
  },

  [MSG.USAGE_REPORT]: async ({ body }) => {
    // Bump the local per-snippet counters first; sync never overwrites
    // `uses`, so these drive the manager display and savings footer.
    for (const kind of ["personal", "library"]) {
      const deltas = body[kind] || [];
      if (!deltas.length) continue;
      const cache = await store.getCache(kind);
      for (const d of deltas) {
        const it = cache.items[d.id];
        if (it) {
          it.uses = (it.uses || 0) + d.delta;
          it.last_used = d.last_used;
        }
      }
      await store.setCache(kind, cache);
    }
    const serverUrl = await store.getServerUrl();
    const token = await store.getToken();
    try {
      await api.reportUsage(serverUrl, token, body);
      return { ok: true };
    } catch (e) {
      // Server telemetry is best-effort; the local bump already stuck.
      return { ok: false, error: e.message };
    }
  },
};

chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  const handler = handlers[msg?.type];
  if (!handler) return false;
  handler(msg).then(sendResponse, (e) => sendResponse({ ok: false, error: String(e) }));
  return true; // async response
});
