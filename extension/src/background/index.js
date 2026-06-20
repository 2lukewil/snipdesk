// Service worker: the only place that holds the JWT and talks to the
// server. UI surfaces (content overlay, popup, manager) send MSG.*
// messages and get a { ok, data } / { ok:false, error } envelope back.

import { MSG } from "../shared/messages.js";
import * as api from "../shared/api.js";
import * as store from "../shared/storage.js";

const SYNC_ALARM = "snipdesk-sync";
const SYNC_PERIOD_MIN = 5;

chrome.runtime.onInstalled.addListener(() => {
  chrome.alarms.create(SYNC_ALARM, { periodInMinutes: SYNC_PERIOD_MIN });
});
chrome.runtime.onStartup.addListener(() => {
  chrome.alarms.create(SYNC_ALARM, { periodInMinutes: SYNC_PERIOD_MIN });
});
chrome.alarms.onAlarm.addListener((a) => {
  if (a.name === SYNC_ALARM) syncNow().catch(() => {});
});

chrome.commands.onCommand.addListener((command) => {
  if (command !== "open-launcher") return;
  chrome.tabs.query({ active: true, currentWindow: true }, (tabs) => {
    const id = tabs[0]?.id;
    if (id != null) {
      chrome.tabs.sendMessage(id, { type: MSG.TOGGLE_LAUNCHER }).catch(() => {});
    }
  });
});

// ---- auth helpers ----

async function authError(e) {
  if (e instanceof api.ApiError && (e.kind === "unauthorized" || e.kind === "inactive")) {
    await store.clearSession();
    return { ok: false, error: e.message, signedOut: true, reason: e.kind };
  }
  return { ok: false, error: e?.message || String(e) };
}

async function afterSignIn(serverUrl, auth) {
  await store.setToken(auth.token);
  await store.setUser(auth.user);
  await store.setSettings({ server_url: serverUrl });
  await syncNow();
  return { ok: true, data: { user: auth.user } };
}

// ---- sync ----

async function pullStream(kind, fetcher) {
  const cache = await store.getCache(kind);
  const res = await fetcher(cache.hwm || 0);
  for (const view of res.snippets || []) {
    if (view.is_deleted) {
      delete cache.items[view.id];
    } else if (view.payload) {
      const prev = cache.items[view.id] || {};
      cache.items[view.id] = {
        id: view.id,
        version: view.version,
        updated_at: view.updated_at,
        ...view.payload,
        uses: prev.uses || 0,
        last_used: prev.last_used || null,
      };
    }
  }
  cache.hwm = res.high_water_mark ?? cache.hwm;
  await store.setCache(kind, cache);
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
    await pullStream("personal", (since) => api.listSnippets(serverUrl, token, since));
    await pullStream("library", (since) => api.listLibrary(serverUrl, token, since));
    return { ok: true };
  } catch (e) {
    return authError(e);
  }
}

// Merged, non-deleted view for the overlay/manager.
async function snippetsForUi() {
  const personal = await store.getCache("personal");
  const library = await store.getCache("library");
  const map = (cache, source) =>
    Object.values(cache.items).map((it) => ({ ...it, source }));
  return [...map(personal, "personal"), ...map(library, "library")];
}

// ---- message router ----

const handlers = {
  [MSG.PING]: async () => ({ ok: true }),

  [MSG.AUTH_STATUS]: async () => {
    const serverUrl = await store.getServerUrl();
    const token = await store.getToken();
    const user = await store.getUser();
    return { ok: true, data: { signedIn: Boolean(token && user), user, serverUrl } };
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
      await syncNow();
      return { ok: true, data: { user: meRes.user } };
    } catch (e) {
      return { ok: false, error: e.message };
    }
  },

  [MSG.AUTH_LOGOUT]: async () => {
    await store.clearSession();
    return { ok: true };
  },

  [MSG.SETTINGS_GET]: async () => ({ ok: true, data: await store.getSettings() }),
  [MSG.SETTINGS_SET]: async ({ patch }) => ({
    ok: true,
    data: await store.setSettings(patch),
  }),

  [MSG.SYNC_NOW]: async () => syncNow(),
  [MSG.SNIPPETS_GET]: async () => ({ ok: true, data: await snippetsForUi() }),

  [MSG.SNIPPET_CREATE]: async ({ payload }) => {
    const serverUrl = await store.getServerUrl();
    const token = await store.getToken();
    const id = crypto.randomUUID();
    try {
      const res = await api.createSnippet(serverUrl, token, id, payload);
      const cache = await store.getCache("personal");
      cache.items[id] = { id, version: res.version, ...payload, uses: 0, last_used: null };
      await store.setCache("personal", cache);
      return { ok: true, data: { id, version: res.version } };
    } catch (e) {
      return authError(e);
    }
  },

  [MSG.SNIPPET_UPDATE]: async ({ id, expectedVersion, payload }) => {
    const serverUrl = await store.getServerUrl();
    const token = await store.getToken();
    try {
      const res = await api.updateSnippet(serverUrl, token, id, expectedVersion, payload);
      const cache = await store.getCache("personal");
      const prev = cache.items[id] || {};
      cache.items[id] = { ...prev, id, version: res.version, ...payload };
      await store.setCache("personal", cache);
      return { ok: true, data: { version: res.version } };
    } catch (e) {
      return authError(e);
    }
  },

  [MSG.SNIPPET_DELETE]: async ({ id }) => {
    const serverUrl = await store.getServerUrl();
    const token = await store.getToken();
    try {
      await api.deleteSnippet(serverUrl, token, id);
      const cache = await store.getCache("personal");
      delete cache.items[id];
      await store.setCache("personal", cache);
      return { ok: true };
    } catch (e) {
      return authError(e);
    }
  },

  [MSG.TRASH_LIST]: async () => {
    const serverUrl = await store.getServerUrl();
    const token = await store.getToken();
    try {
      return { ok: true, data: await api.listTrash(serverUrl, token) };
    } catch (e) {
      return authError(e);
    }
  },

  [MSG.TRASH_RESTORE]: async ({ id }) => {
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
    const serverUrl = await store.getServerUrl();
    const token = await store.getToken();
    try {
      await api.reportUsage(serverUrl, token, body);
      return { ok: true };
    } catch (e) {
      // Telemetry is best-effort; never surface as a hard failure.
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
