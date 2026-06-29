// chrome.storage.local accessors. The worker is the only writer for
// auth + caches; UI surfaces read settings directly where convenient.

// Inline formatting markup, mirroring the desktop app's defaults (which is
// what WHMCS expects). Each rule wraps text with prefix/suffix; the preview
// renders these as formatted text. Editable in Settings.
export const DEFAULT_FORMAT_RULES = [
  { label: "Bold", prefix: "**", suffix: "**" },
  { label: "Italic", prefix: "*", suffix: "*" },
  { label: "Code", prefix: "`", suffix: "`" },
  { label: "Link", prefix: "[", suffix: "](https://)" },
];

const DEFAULT_SETTINGS = {
  server_url: "",
  show_savings_estimate: false,
  typing_speed_wpm: 40,
  hourly_wage: 0,
  wage_currency: "$",
  sort_by_usage: true,
  show_usage_count: true,
  theme: "dark",
  accent_color: "",
  compact: false,
  onboarded: false,
  format_rules: DEFAULT_FORMAT_RULES,
};

const get = (keys) =>
  new Promise((resolve) => chrome.storage.local.get(keys, resolve));
const set = (obj) =>
  new Promise((resolve) => chrome.storage.local.set(obj, resolve));
const remove = (keys) =>
  new Promise((resolve) => chrome.storage.local.remove(keys, resolve));

export async function getToken() {
  return (await get("token")).token ?? null;
}
export async function setToken(token) {
  await set({ token });
}
export async function clearToken() {
  await remove("token");
}

export async function getServerUrl() {
  return (await getSettings()).server_url;
}

export async function getSettings() {
  const { settings } = await get("settings");
  return { ...DEFAULT_SETTINGS, ...(settings ?? {}) };
}
export async function setSettings(patch) {
  const merged = { ...(await getSettings()), ...patch };
  await set({ settings: merged });
  return merged;
}

export async function getUser() {
  return (await get("user")).user ?? null;
}
export async function setUser(user) {
  await set({ user });
}
export async function clearUser() {
  await remove("user");
}

// Snippet caches: personal + library, each with its sync high-water
// mark. Stored as id-keyed maps so applying a sync delta is a merge.
export async function getCache(kind) {
  const key = `cache_${kind}`;
  const data = (await get(key))[key];
  return data ?? { items: {}, hwm: 0 };
}
export async function setCache(kind, cache) {
  await set({ [`cache_${kind}`]: cache });
}

export async function getVarHistory() {
  return (await get("var_history")).var_history ?? {};
}
export async function setVarHistory(history) {
  await set({ var_history: history });
}

// Empty personal folders created in the manager. The server has no
// personal-folders table, so an empty folder only lives here until a
// snippet is filed into it (after which folder_path carries it).
export async function getPendingFolders() {
  return (await get("pending_folders")).pending_folders ?? [];
}
export async function setPendingFolders(list) {
  await set({ pending_folders: list });
}

// Manual folder ordering: path -> sort index within its sibling group.
// Folders without an entry fall back to alphabetical. Local-only, same
// as pending folders.
export async function getFolderOrder() {
  return (await get("folder_order")).folder_order ?? {};
}
export async function setFolderOrder(map) {
  await set({ folder_order: map });
}

export async function getSavings() {
  return (await get("savings")).savings ?? { team_chars: 0 };
}
export async function setSavings(savings) {
  await set({ savings });
}

// Last sync outcome, surfaced in the popup. { at, ok, error }.
export async function getSyncStatus() {
  return (await get("sync_status")).sync_status ?? null;
}
export async function setSyncStatus(status) {
  await set({ sync_status: status });
}

// Text captured via the page context menu, picked up by the manager to
// prefill a new snippet. One-shot: the manager clears it on read.
export async function setPendingNewSnippet(text) {
  await set({ pending_new_snippet: text });
}
export async function getPendingNewSnippet() {
  return (await get("pending_new_snippet")).pending_new_snippet ?? null;
}
export async function clearPendingNewSnippet() {
  await remove("pending_new_snippet");
}

export async function clearSession() {
  // Personal snippets stay on the device so the extension is fully usable
  // signed out / offline, matching the desktop. Only the auth token, the
  // cached identity, and the team library (which only exists while signed
  // in) are dropped. cache_personal keeps its sync high-water mark so the
  // same user re-signing in resumes a delta sync and any edits made while
  // signed out still push up.
  await remove(["token", "user", "cache_library"]);
}

// Admin-managed config from enterprise policy (chrome.storage.managed).
// Empty {} unless a managed-storage policy is deployed for this
// extension. Readable from any extension context.
export async function getManaged() {
  return new Promise((resolve) => {
    try {
      chrome.storage.managed.get(null, (v) =>
        resolve(chrome.runtime.lastError ? {} : v || {}),
      );
    } catch {
      resolve({});
    }
  });
}
