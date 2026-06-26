import { MSG, send } from "../shared/messages.js";
import { getManaged } from "../shared/storage.js";

const $ = (id) => document.getElementById(id);
const statusEl = $("status");
const errorEl = $("error");
const signedOut = $("signed-out");
const signedIn = $("signed-in");
const identity = $("identity");

function showError(msg) {
  errorEl.textContent = msg;
  errorEl.classList.remove("hidden");
}
function clearError() {
  errorEl.textContent = "";
  errorEl.classList.add("hidden");
}

async function refresh() {
  const res = await send(MSG.AUTH_STATUS);
  if (!res.ok) {
    statusEl.textContent = "Background worker not responding.";
    return;
  }
  const { signedIn: isIn, user, serverUrl } = res.data;
  // Local snippets work without an account, so counts/savings/launch are
  // always shown; sign-in only unlocks the team library and sync.
  loadCounts();
  if (isIn) {
    statusEl.textContent = "Signed in";
    statusEl.classList.add("ok");
    signedOut.classList.add("hidden");
    signedIn.classList.remove("hidden");
    identity.classList.remove("hidden");
    $("who-name").textContent = user?.display_name || "";
    $("who-email").textContent = user?.email || "";
    renderSyncStatus(res.data.lastSync, res.data.pending);
  } else {
    statusEl.textContent = "Local only";
    statusEl.classList.remove("ok");
    signedIn.classList.add("hidden");
    identity.classList.add("hidden");
    signedOut.classList.remove("hidden");
    // A policy-pinned URL locks the field; agents just pick a method.
    const pinned = ((await getManaged()).server_url || "").trim();
    const url = pinned || serverUrl;
    $("server-url").value = url || "";
    $("server-url").disabled = !!pinned;
    // Options stay hidden until loadMethods confirms the server answers;
    // an empty or unreachable URL shows no sign-in options at all.
    if (url) loadMethods(url);
    else hideAuthOptions();
  }
}

function hideAuthOptions() {
  $("auth-options").classList.add("hidden");
}

function relTime(ms) {
  const diff = (Date.now() - ms) / 1000;
  if (diff < 60) return "just now";
  if (diff < 3600) return `${Math.round(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.round(diff / 3600)}h ago`;
  return `${Math.round(diff / 86400)}d ago`;
}

function renderSyncStatus(last, pending) {
  const el = $("sync-status");
  const queued = pending ? ` ${pending} change${pending > 1 ? "s" : ""} pending.` : "";
  if (!last) {
    el.textContent = ("Not synced yet." + queued).trim();
    el.classList.toggle("err", !!pending);
    return;
  }
  if (last.ok) {
    el.textContent = `Last synced ${relTime(last.at)}.${queued}`;
    el.classList.toggle("err", !!pending);
  } else {
    el.textContent = `Last sync failed ${relTime(last.at)}.${queued}`;
    el.classList.add("err");
  }
}

async function loadCounts() {
  const res = await send(MSG.SNIPPETS_GET);
  if (!res.ok) return;
  const all = res.data || [];
  const personal = all.filter((s) => s.source === "personal").length;
  const library = all.filter((s) => s.source === "library").length;
  $("counts").textContent = `${personal} personal, ${library} library`;
  const sres = await send(MSG.SETTINGS_GET);
  renderSavings(all, sres.ok ? sres.data : {});
}

function fmtDuration(sec) {
  if (sec < 60) return `${Math.round(sec)}s`;
  if (sec < 3600) return `${Math.round(sec / 60)}m`;
  if (sec < 86400) return `${(sec / 3600).toFixed(1)}h`;
  return `${(sec / 86400).toFixed(1)}d`;
}

function renderSavings(all, settings) {
  const el = $("popup-savings");
  if (!settings.show_savings_estimate) {
    el.textContent = "";
    return;
  }
  const wpm = settings.typing_speed_wpm || 40;
  const totalChars = all.reduce((sum, s) => sum + (s.uses || 0) * (s.body || "").length, 0);
  const seconds = ((totalChars / 5) / wpm) * 60;
  let text = `Saved ~${fmtDuration(seconds)}`;
  const wage = settings.hourly_wage || 0;
  if (wage > 0) text += ` / ${settings.wage_currency || "$"}${((seconds / 3600) * wage).toFixed(2)}`;
  el.textContent = text;
}

// Fetch the server's configured sign-in methods and render: password
// fields when enabled, a button per OIDC provider. Called when the
// server URL is set.
async function loadMethods(serverUrl) {
  const providers = $("providers");
  providers.replaceChildren();
  if (!serverUrl) {
    hideAuthOptions();
    return;
  }
  const res = await send(MSG.AUTH_METHODS, { serverUrl });
  if (!res.ok) {
    // Unreachable / not a SnipDesk server: show no sign-in options.
    hideAuthOptions();
    return;
  }
  const methods = res.data || {};
  const hasPassword = !!methods.password?.enabled;
  const hasProviders = (methods.providers || []).length > 0;
  // A reachable server with no configured methods has nothing to offer.
  if (!hasPassword && !hasProviders) {
    hideAuthOptions();
    return;
  }
  // Reachable: reveal the options and render only what the server offers.
  $("auth-options").classList.remove("hidden");
  $("password-section").classList.toggle("hidden", !hasPassword);
  // Token paste only applies when the server has SSO providers (the token
  // comes from completing one). Collapse it whenever it isn't shown.
  $("sso-token").classList.toggle("hidden", !hasProviders);
  if (!hasProviders) $("token-section").classList.add("hidden");
  for (const p of methods.providers || []) {
    const btn = document.createElement("button");
    btn.textContent = p.display_name || `Sign in with ${p.id}`;
    btn.addEventListener("click", async () => {
      clearError();
      btn.disabled = true;
      const r = await send(MSG.AUTH_SSO, { serverUrl, startUrl: p.start_url });
      btn.disabled = false;
      if (!r.ok) {
        showError(r.error || "Sign-in failed.");
        return;
      }
      refresh();
    });
    providers.appendChild(btn);
  }
}

// Re-check the server as the URL is typed (debounced) and on blur. The
// options only appear once the server actually answers, so a half-typed
// or unreachable URL shows nothing.
let methodsTimer;
$("server-url").addEventListener("input", () => {
  const v = $("server-url").value.trim();
  if (!v) {
    hideAuthOptions();
    return;
  }
  clearTimeout(methodsTimer);
  methodsTimer = setTimeout(() => loadMethods($("server-url").value.trim()), 400);
});
$("server-url").addEventListener("blur", () => {
  clearTimeout(methodsTimer);
  loadMethods($("server-url").value.trim());
});

$("toggle-token").addEventListener("click", () => {
  $("token-section").classList.toggle("hidden");
});

$("btn-login").addEventListener("click", async () => {
  clearError();
  const serverUrl = $("server-url").value.trim();
  const email = $("email").value.trim();
  const password = $("password").value;
  if (!serverUrl || !email || !password) {
    showError("Server URL, email, and password are required.");
    return;
  }
  $("btn-login").disabled = true;
  $("btn-login").textContent = "Signing in...";
  const res = await send(MSG.AUTH_LOGIN, { serverUrl, email, password });
  $("btn-login").disabled = false;
  $("btn-login").textContent = "Sign in";
  if (!res.ok) {
    showError(res.error || "Sign-in failed.");
    return;
  }
  refresh();
});

$("btn-token").addEventListener("click", async () => {
  clearError();
  const serverUrl = $("server-url").value.trim();
  const token = $("token").value.trim();
  if (!serverUrl || !token) {
    showError("Server URL and token are required.");
    return;
  }
  const res = await send(MSG.AUTH_PASTE_TOKEN, { serverUrl, token });
  if (!res.ok) {
    showError(res.error || "Token rejected.");
    return;
  }
  refresh();
});

$("btn-sync").addEventListener("click", async () => {
  $("btn-sync").disabled = true;
  $("btn-sync").textContent = "Syncing...";
  const res = await send(MSG.SYNC_NOW);
  $("btn-sync").disabled = false;
  $("btn-sync").textContent = "Sync now";
  if (!res.ok && !res.signedOut) showError(res.error || "Sync failed.");
  // Re-read status so the last-synced time and pending count update.
  refresh();
});

$("btn-logout").addEventListener("click", async () => {
  await send(MSG.AUTH_LOGOUT);
  refresh();
});

$("btn-launch").addEventListener("click", async () => {
  await send(MSG.LAUNCH_HERE);
  window.close(); // let the in-page overlay take focus
});

$("open-manager").addEventListener("click", () => chrome.runtime.openOptionsPage());

async function applyTheme() {
  const res = await send(MSG.SETTINGS_GET);
  const theme = res.ok ? res.data?.theme : "dark";
  document.documentElement.dataset.theme = theme === "light" ? "light" : "dark";
}

applyTheme();
refresh();
