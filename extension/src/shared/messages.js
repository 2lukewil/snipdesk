// Message types exchanged between the content script / popup / manager
// and the background worker. The worker owns all network + auth; UI
// surfaces only ever send these.
export const MSG = {
  PING: "ping",
  TOGGLE_LAUNCHER: "toggle-launcher",
  LAUNCH_HERE: "launch-here",

  AUTH_STATUS: "auth-status",
  AUTH_METHODS: "auth-methods",
  AUTH_LOGIN: "auth-login",
  AUTH_SIGNUP: "auth-signup",
  AUTH_PASTE_TOKEN: "auth-paste-token",
  AUTH_SSO: "auth-sso",
  AUTH_LOGOUT: "auth-logout",

  SETTINGS_GET: "settings-get",
  SETTINGS_SET: "settings-set",

  SYNC_NOW: "sync-now",
  SNIPPETS_GET: "snippets-get",
  SNIPPET_CREATE: "snippet-create",
  SNIPPET_UPDATE: "snippet-update",
  SNIPPET_DELETE: "snippet-delete",
  SNIPPET_DEDUPE: "snippet-dedupe",

  TRASH_LIST: "trash-list",
  TRASH_RESTORE: "trash-restore",

  USAGE_REPORT: "usage-report",
  VAR_HISTORY_GET: "var-history-get",
  VAR_HISTORY_ADD: "var-history-add",
};

// Promise wrapper over chrome.runtime.sendMessage with a uniform
// { ok, data } / { ok:false, error } envelope.
export function send(type, payload) {
  return new Promise((resolve) => {
    chrome.runtime.sendMessage({ type, ...payload }, (res) => {
      if (chrome.runtime.lastError) {
        resolve({ ok: false, error: chrome.runtime.lastError.message });
      } else {
        resolve(res ?? { ok: false, error: "no response" });
      }
    });
  });
}
