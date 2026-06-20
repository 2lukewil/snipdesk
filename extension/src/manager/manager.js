import { MSG, send } from "../shared/messages.js";
import { filterSnippets, sortSnippets } from "../shared/search.js";
import { validateSnippet } from "../shared/validate.js";

const $ = (id) => document.getElementById(id);
const el = (tag, cls, text) => {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text != null) n.textContent = text;
  return n;
};

let settings = {};
let snippets = [];
let selectedId = null;

async function init() {
  const status = (await send(MSG.AUTH_STATUS)).data || {};
  if (!status.signedIn) {
    $("signed-out").classList.remove("hidden");
    return;
  }
  $("tabs").classList.remove("hidden");
  $("btn-logout").classList.remove("hidden");
  $("identity").textContent = status.user?.display_name || status.user?.email || "";

  settings = (await send(MSG.SETTINGS_GET)).data || {};
  await loadSnippets();
  fillSettingsForm();
  wire();
  showTab("snippets");
}

async function loadSnippets() {
  snippets = (await send(MSG.SNIPPETS_GET)).data || [];
  renderFolderFilter();
  renderList();
  renderSavings();
}

// ---- tabs ----
function showTab(name) {
  for (const btn of document.querySelectorAll("#tabs button")) {
    btn.classList.toggle("active", btn.dataset.tab === name);
  }
  for (const sec of document.querySelectorAll(".tab")) {
    sec.classList.toggle("hidden", sec.id !== `tab-${name}`);
  }
  if (name === "trash") loadTrash();
}

// ---- snippet list ----
function renderFolderFilter() {
  const sel = $("folder-filter");
  const current = sel.value;
  const folders = [...new Set(snippets.map((s) => s.folder_path).filter(Boolean))].sort();
  sel.replaceChildren(el("option", null, "All folders"));
  sel.firstChild.value = "";
  for (const f of folders) {
    const o = el("option", null, f);
    o.value = f;
    sel.appendChild(o);
  }
  sel.value = folders.includes(current) ? current : "";
}

function visibleSnippets() {
  const q = $("search").value;
  const folder = $("folder-filter").value;
  let list = filterSnippets(snippets, q);
  if (folder) list = list.filter((s) => s.folder_path === folder);
  if (!q.trim()) list = sortSnippets(list, settings.sort_by_usage !== false);
  return list;
}

function renderList() {
  const list = $("list");
  list.replaceChildren();
  const items = visibleSnippets();
  if (items.length === 0) {
    list.appendChild(el("div", "row muted", "No snippets."));
    return;
  }
  for (const s of items) {
    const row = el("div", "row");
    row.classList.toggle("active", s.id === selectedId);
    const t = el("div", "t");
    t.appendChild(document.createTextNode(s.title || "(untitled)"));
    if (s.source === "library") t.appendChild(el("span", "badge", "team"));
    row.appendChild(t);
    const metaBits = [];
    if (s.folder_path) metaBits.push(s.folder_path);
    if (settings.show_usage_count !== false && s.uses) metaBits.push(`${s.uses} uses`);
    if (metaBits.length) row.appendChild(el("div", "meta", metaBits.join("  -  ")));
    row.addEventListener("click", () => openEditor(s));
    list.appendChild(row);
  }
}

// ---- editor ----
function openEditor(snippet) {
  selectedId = snippet ? snippet.id : null;
  renderList();
  const editor = $("editor");
  editor.replaceChildren();
  const readOnly = snippet?.source === "library";

  if (readOnly) {
    editor.appendChild(el("div", "ro-note", "Team library snippet (read-only; managed in the dashboard)."));
  }

  const title = inputField(editor, "Title", snippet?.title || "", readOnly);
  const folder = inputField(editor, "Folder (optional)", snippet?.folder_path || "", readOnly);
  const tags = inputField(editor, "Tags (comma-separated)", (snippet?.tags || []).join(", "), readOnly);

  const bodyLabel = el("label", null, "Body");
  const body = el("textarea");
  body.value = snippet?.body || "";
  body.disabled = readOnly;
  bodyLabel.appendChild(body);
  editor.appendChild(bodyLabel);

  const err = el("div", "error hidden");
  editor.appendChild(err);

  if (readOnly) return;

  const actions = el("div", "actions");
  const saveBtn = el("button", "primary", snippet ? "Save" : "Create");
  actions.appendChild(saveBtn);
  if (snippet) {
    const dupBtn = el("button", null, "Duplicate");
    const delBtn = el("button", "danger", "Delete");
    actions.appendChild(dupBtn);
    actions.appendChild(delBtn);
    dupBtn.addEventListener("click", () => duplicate(snippet));
    delBtn.addEventListener("click", () => remove(snippet));
  }
  editor.appendChild(actions);

  saveBtn.addEventListener("click", async () => {
    const payload = {
      title: title.value,
      body: body.value,
      tags: tags.value.split(",").map((t) => t.trim()).filter(Boolean),
      folder_path: folder.value.trim() || null,
    };
    const problem = validateSnippet(payload);
    if (problem) {
      err.textContent = problem;
      err.classList.remove("hidden");
      return;
    }
    saveBtn.disabled = true;
    const res = snippet
      ? await send(MSG.SNIPPET_UPDATE, { id: snippet.id, expectedVersion: snippet.version, payload })
      : await send(MSG.SNIPPET_CREATE, { payload });
    saveBtn.disabled = false;
    if (!res.ok) {
      err.textContent = res.error || "Save failed.";
      err.classList.remove("hidden");
      return;
    }
    selectedId = snippet ? snippet.id : res.data?.id;
    await loadSnippets();
    const fresh = snippets.find((s) => s.id === selectedId);
    if (fresh) openEditor(fresh);
  });
}

function inputField(parent, labelText, value, disabled) {
  const label = el("label", null, labelText);
  const input = el("input");
  input.type = "text";
  input.value = value;
  input.disabled = disabled;
  label.appendChild(input);
  parent.appendChild(label);
  return input;
}

async function duplicate(snippet) {
  const payload = {
    title: `${snippet.title} (copy)`,
    body: snippet.body,
    tags: snippet.tags || [],
    folder_path: snippet.folder_path || null,
  };
  const res = await send(MSG.SNIPPET_CREATE, { payload });
  if (res.ok) {
    selectedId = res.data?.id;
    await loadSnippets();
    const fresh = snippets.find((s) => s.id === selectedId);
    if (fresh) openEditor(fresh);
  }
}

async function remove(snippet) {
  if (!confirm(`Delete "${snippet.title}"? It can be restored from Trash.`)) return;
  const res = await send(MSG.SNIPPET_DELETE, { id: snippet.id });
  if (res.ok) {
    selectedId = null;
    await loadSnippets();
    $("editor").replaceChildren(el("p", "muted", "Select a snippet, or create a new one."));
  }
}

// ---- settings ----
function fillSettingsForm() {
  $("set-show-savings").checked = !!settings.show_savings_estimate;
  $("set-wpm").value = settings.typing_speed_wpm ?? 40;
  $("set-wage").value = settings.hourly_wage ?? 0;
  $("set-currency").value = settings.wage_currency ?? "$";
  $("set-sort-usage").checked = settings.sort_by_usage !== false;
  $("set-usage-count").checked = settings.show_usage_count !== false;
}

async function saveSettings() {
  const patch = {
    show_savings_estimate: $("set-show-savings").checked,
    typing_speed_wpm: Number($("set-wpm").value) || 40,
    hourly_wage: Number($("set-wage").value) || 0,
    wage_currency: $("set-currency").value || "$",
    sort_by_usage: $("set-sort-usage").checked,
    show_usage_count: $("set-usage-count").checked,
  };
  const res = await send(MSG.SETTINGS_SET, { patch });
  const status = $("settings-status");
  if (res.ok) {
    settings = res.data;
    status.textContent = "Saved.";
    status.className = "status ok";
    renderList();
    renderSavings();
  } else {
    status.textContent = res.error || "Save failed.";
    status.className = "status err";
  }
}

// ---- savings ----
function renderSavings() {
  const out = $("savings");
  if (!settings.show_savings_estimate) {
    out.textContent = "";
    return;
  }
  const wpm = settings.typing_speed_wpm || 40;
  const totalChars = snippets.reduce((sum, s) => sum + (s.uses || 0) * (s.body || "").length, 0);
  const seconds = ((totalChars / 5) / wpm) * 60;
  let text = `Saved ~${formatDuration(seconds)}`;
  const wage = settings.hourly_wage || 0;
  if (wage > 0) text += ` / ${settings.wage_currency || "$"}${((seconds / 3600) * wage).toFixed(2)}`;
  out.textContent = text;
}

function formatDuration(sec) {
  if (sec < 60) return `${Math.round(sec)}s`;
  if (sec < 3600) return `${Math.round(sec / 60)}m`;
  if (sec < 86400) return `${(sec / 3600).toFixed(1)}h`;
  return `${(sec / 86400).toFixed(1)}d`;
}

// ---- trash ----
async function loadTrash() {
  const list = $("trash-list");
  list.replaceChildren(el("div", "row muted", "Loading..."));
  const res = await send(MSG.TRASH_LIST);
  list.replaceChildren();
  if (!res.ok) {
    list.appendChild(el("div", "row muted", res.error || "Could not load trash."));
    return;
  }
  const items = res.data || [];
  if (items.length === 0) {
    list.appendChild(el("div", "row muted", "Trash is empty."));
    return;
  }
  for (const item of items) {
    const row = el("div", "row");
    row.appendChild(el("div", "t", item.payload?.title || "(untitled)"));
    const restore = el("button", null, "Restore");
    restore.addEventListener("click", async () => {
      restore.disabled = true;
      const r = await send(MSG.TRASH_RESTORE, { id: item.id });
      if (r.ok) {
        await loadSnippets();
        loadTrash();
      } else {
        restore.disabled = false;
      }
    });
    row.appendChild(restore);
    list.appendChild(row);
  }
}

// ---- wiring ----
function wire() {
  for (const btn of document.querySelectorAll("#tabs button")) {
    btn.addEventListener("click", () => showTab(btn.dataset.tab));
  }
  $("search").addEventListener("input", renderList);
  $("folder-filter").addEventListener("change", renderList);
  $("btn-new").addEventListener("click", () => openEditor(null));
  $("btn-save-settings").addEventListener("click", saveSettings);
  $("btn-logout").addEventListener("click", async () => {
    await send(MSG.AUTH_LOGOUT);
    location.reload();
  });
}

init();
