// HTTP client for the SnipDesk server, mirroring the desktop client
// (crates/snipdesk-teams/src/api.rs). Pure functions: no chrome.*,
// callable from the worker. Write bodies are FLAT (id/expected_version
// alongside the payload fields), matching the server's #[serde(flatten)].

export class ApiError extends Error {
  constructor(kind, { status, code, message } = {}) {
    super(message || code || kind);
    this.kind = kind; // network | unauthorized | inactive | conflict | server
    this.status = status;
    this.code = code;
  }
}

function base(serverUrl) {
  return (serverUrl || "").trim().replace(/\/+$/, "");
}

async function request(serverUrl, path, { method = "GET", token, body } = {}) {
  const headers = {};
  if (token) headers.authorization = `Bearer ${token}`;
  if (body !== undefined) headers["content-type"] = "application/json";

  let res;
  try {
    res = await fetch(base(serverUrl) + path, {
      method,
      headers,
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
  } catch (e) {
    throw new ApiError("network", { message: e.message });
  }

  if (res.ok) {
    if (res.status === 204) return null;
    const text = await res.text();
    return text ? JSON.parse(text) : null;
  }

  let code = "server_error";
  let message = `HTTP ${res.status}`;
  try {
    const parsed = await res.json();
    if (parsed?.error) code = parsed.error;
    if (parsed?.message) message = parsed.message;
  } catch {
    // non-JSON error body; keep the generic message
  }

  if (res.status === 401) throw new ApiError("unauthorized", { status: 401, code, message });
  if (res.status === 403 && (code === "account_disabled" || code === "account_gone")) {
    throw new ApiError("inactive", { status: 403, code, message });
  }
  if (res.status === 409 && code === "version_conflict") {
    throw new ApiError("conflict", { status: 409, code, message });
  }
  throw new ApiError("server", { status: res.status, code, message });
}

// ---- Auth ----
export const authMethods = (s) => request(s, "/api/auth/methods");
export const signup = (s, email, password, display_name) =>
  request(s, "/api/auth/signup", { method: "POST", body: { email, password, display_name } });
export const login = (s, email, password) =>
  request(s, "/api/auth/login", { method: "POST", body: { email, password } });
export const me = (s, token) => request(s, "/api/me", { token });
export const clientConfig = (s, token) => request(s, "/api/client-config", { token });
export const updateMe = (s, token, patch) =>
  request(s, "/api/me", { method: "PATCH", token, body: patch });

// ---- Personal snippets ----
export const listSnippets = (s, token, since = 0) =>
  request(s, `/api/snippets?since=${since}`, { token });
export const createSnippet = (s, token, id, payload) =>
  request(s, "/api/snippets", { method: "POST", token, body: { id, ...payload } });
export const updateSnippet = (s, token, id, expected_version, payload) =>
  request(s, `/api/snippets/${encodeURIComponent(id)}`, {
    method: "PUT",
    token,
    body: { expected_version, ...payload },
  });
export const deleteSnippet = (s, token, id) =>
  request(s, `/api/snippets/${encodeURIComponent(id)}`, { method: "DELETE", token });

// ---- Trash ----
export const listTrash = (s, token) => request(s, "/api/snippets/trash", { token });
export const restoreTrash = (s, token, id) =>
  request(s, `/api/snippets/${encodeURIComponent(id)}/restore`, { method: "POST", token });

// ---- Team library (read-only from the extension; managed via dashboard) ----
export const listLibrary = (s, token, since = 0) =>
  request(s, `/api/library?since=${since}`, { token });

// ---- Telemetry ----
export const reportUsage = (s, token, body) =>
  request(s, "/api/usage/report", { method: "POST", token, body });
