import { MSG, send } from "../shared/messages.js";

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
  if (isIn) {
    statusEl.textContent = "Signed in";
    statusEl.classList.add("ok");
    signedOut.classList.add("hidden");
    signedIn.classList.remove("hidden");
    $("who-name").textContent = user?.display_name || "";
    $("who-email").textContent = user?.email || "";
    loadCounts();
  } else {
    statusEl.textContent = "Not signed in";
    statusEl.classList.remove("ok");
    signedIn.classList.add("hidden");
    signedOut.classList.remove("hidden");
    if (serverUrl) {
      $("server-url").value = serverUrl;
      loadMethods(serverUrl);
    }
  }
}

async function loadCounts() {
  const res = await send(MSG.SNIPPETS_GET);
  if (!res.ok) return;
  const all = res.data || [];
  const personal = all.filter((s) => s.source === "personal").length;
  const library = all.filter((s) => s.source === "library").length;
  $("counts").textContent = `${personal} personal, ${library} library`;
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
  if (res.signedOut) {
    refresh();
    return;
  }
  loadCounts();
});

$("btn-logout").addEventListener("click", async () => {
  await send(MSG.AUTH_LOGOUT);
  refresh();
});

$("open-manager").addEventListener("click", () => chrome.runtime.openOptionsPage());

refresh();
