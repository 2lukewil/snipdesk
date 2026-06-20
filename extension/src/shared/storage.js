// chrome.storage.local accessors. The worker is the only writer for
// auth + caches; UI surfaces read settings directly where convenient.

const DEFAULT_SETTINGS = {
  server_url: "",
  show_savings_estimate: false,
  typing_speed_wpm: 40,
  hourly_wage: 0,
  wage_currency: "$",
  sort_by_usage: true,
  show_usage_count: true,
  theme: "dark",
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

export async function getSavings() {
  return (await get("savings")).savings ?? { team_chars: 0 };
}
export async function setSavings(savings) {
  await set({ savings });
}

export async function clearSession() {
  await remove(["token", "user", "cache_personal", "cache_library"]);
}
