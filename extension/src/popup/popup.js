import { MSG, send } from "../shared/messages.js";
import { getManaged } from "../shared/storage.js";

const $ = (id) => document.getElementById(id);
const statusEl = $("status");
const errorEl = $("error");
const signedOut = $("signed-out");
const signedIn = $("signed-in");

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
    $("who-name").textContent = user?.display_name || "";
    $("who-email").textContent = user?.email || "";
    renderSyncStatus(res.data.lastSync, res.data.pending);
  } else {
    statusEl.textContent = "Local only";
    statusEl.classList.remove("ok");
    signedIn.classList.add("hidden");
    signedOut.classList.remove("hidden");
    // A policy-pinned URL locks the field; agents just pick a method.
    const pinned = ((await getManaged()).server_url || "").trim();
    const url = pinned || serverUrl;
    if (url) {
      $("server-url").value = url;
      $("server-url").disabled = !!pinned;
      loadMethods(url);
    }
  }
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
  if (!serverUrl) return;
  const res = await send(MSG.AUTH_METHODS, { serverUrl });
  if (!res.ok) return; // unreachable server; leave password form as-is
  const methods = res.data || {};
  $("password-section").classList.toggle("hidden", !(methods.password?.enabled));
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

$("server-url").addEventListener("blur", () => loadMethods($("server-url").value.trim()));

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
