import { MSG, send } from "../shared/messages.js";
import { filterSnippets, sortSnippets } from "../shared/search.js";
import { extractVarNames, substitute, splitForPreview } from "../shared/variables.js";
import overlayCss from "./overlay.css?inline";

// Insertion target captured BEFORE the overlay steals focus. For
// inputs/textareas we also snapshot the selection offsets; for
// contenteditable we clone the live Range (focus moves to the overlay
// search box, so the page selection would otherwise be lost).
let target = null;

let host = null; // overlay host element
let root = null; // its shadow root
let snippets = [];
let filtered = [];
let selected = 0;
let mode = "list"; // "list" | "vars"
let pending = null; // snippet awaiting variable fill
let settingsCache = {};

function applyFilter(query) {
  filtered = query.trim()
    ? filterSnippets(snippets, query) // already rank-ordered
    : sortSnippets(snippets, settingsCache.sort_by_usage !== false);
  selected = 0;
}

function captureTarget() {
  const el = document.activeElement;
  if (!el) return null;
  const tag = el.tagName?.toLowerCase();
  if (tag === "input" || tag === "textarea") {
    return { el, kind: "input", start: el.selectionStart, end: el.selectionEnd };
  }
  if (el.isContentEditable) {
    const sel = window.getSelection();
    let range = null;
    if (sel && sel.rangeCount && el.contains(sel.anchorNode)) {
      range = sel.getRangeAt(0).cloneRange();
    }
    return { el, kind: "ce", range };
  }
  return null;
}

function insertIntoTarget(text) {
  if (!target) return false;
  const { el, kind } = target;
  el.focus();
  if (kind === "input") {
    const v = el.value;
    const start = target.start ?? v.length;
    const end = target.end ?? v.length;
    el.value = v.slice(0, start) + text + v.slice(end);
    const caret = start + text.length;
    el.setSelectionRange(caret, caret);
    el.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: text }));
    return true;
  }
  // contenteditable
  const sel = window.getSelection();
  if (target.range) {
    sel.removeAllRanges();
    sel.addRange(target.range);
  }
  let ok = false;
  try {
    ok = document.execCommand("insertText", false, text);
  } catch {
    ok = false;
  }
  if (!ok && sel.rangeCount) {
    // Fallback for editors where execCommand is disabled.
    const r = sel.getRangeAt(0);
    r.deleteContents();
    const node = document.createTextNode(text);
    r.insertNode(node);
    r.setStartAfter(node);
    r.collapse(true);
    sel.removeAllRanges();
    sel.addRange(r);
    el.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: text }));
    ok = true;
  }
  return ok;
}

function close() {
  if (host) {
    host.remove();
    host = null;
    root = null;
  }
  mode = "list";
  pending = null;
}

function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text != null) n.textContent = text;
  return n;
}

// Title with the query substring marked, built without innerHTML.
function titleNode(title, query) {
  const wrap = el("div", "t");
  const q = (query || "").trim().toLowerCase();
  const idx = q ? title.toLowerCase().indexOf(q) : -1;
  if (idx < 0) {
    wrap.textContent = title;
  } else {
    wrap.appendChild(document.createTextNode(title.slice(0, idx)));
    const mk = el("mark", null, title.slice(idx, idx + q.length));
    wrap.appendChild(mk);
    wrap.appendChild(document.createTextNode(title.slice(idx + q.length)));
  }
  return wrap;
}

function renderList(query) {
  const list = root.querySelector(".sd-list");
  const preview = root.querySelector(".sd-preview");
  list.replaceChildren();
  if (filtered.length === 0) {
    const empty = el("div", "sd-empty", snippets.length === 0
      ? "No snippets yet. Sign in and sync from the toolbar popup."
      : "No matches.");
    list.appendChild(empty);
    preview.replaceChildren();
    return;
  }
  filtered.forEach((s, i) => {
    const row = el("div", "sd-row");
    row.setAttribute("role", "option");
    row.setAttribute("aria-selected", String(i === selected));
    row.appendChild(titleNode(s.title || "(untitled)", query));
    const metaBits = [];
    if (s.source === "library") metaBits.push("team");
    if (s.folder_path) metaBits.push(s.folder_path);
    if ((s.tags || []).length) metaBits.push((s.tags || []).join(", "));
    if (metaBits.length) row.appendChild(el("div", "meta", metaBits.join("  -  ")));
    row.addEventListener("mousedown", (e) => {
      e.preventDefault();
      selected = i;
      choose();
    });
    list.appendChild(row);
  });
  renderPreview();
}

function renderPreview() {
  const preview = root.querySelector(".sd-preview");
  preview.replaceChildren();
  const s = filtered[selected];
  if (!s) return;
  for (const chunk of splitForPreview(s.body || "")) {
    if (chunk.type === "text") {
      preview.appendChild(document.createTextNode(chunk.text));
    } else {
      preview.appendChild(el("var", null, `{${chunk.name}}`));
    }
  }
  const sel = root.querySelector(`.sd-row[aria-selected="true"]`);
  sel?.scrollIntoView({ block: "nearest" });
}

function setSelected(i) {
  if (filtered.length === 0) return;
  selected = (i + filtered.length) % filtered.length;
  root.querySelectorAll(".sd-row").forEach((r, idx) =>
    r.setAttribute("aria-selected", String(idx === selected)),
  );
  renderPreview();
}

async function choose() {
  const s = filtered[selected];
  if (!s) return;
  const vars = extractVarNames(s.body || "");
  if (vars.length === 0) {
    insertIntoTarget(s.body || "");
    close();
    return;
  }
  pending = s;
  await showVarPrompt(s, vars);
}

async function showVarPrompt(s, vars) {
  mode = "vars";
  const panel = root.querySelector(".sd-panel");
  panel.querySelector(".sd-search").classList.add("hidden");
  panel.querySelector(".sd-list").classList.add("hidden");
  panel.querySelector(".sd-preview").classList.add("hidden");
  const varsView = panel.querySelector(".sd-vars");
  varsView.classList.remove("hidden");
  varsView.replaceChildren();

  varsView.appendChild(el("h3", null, s.title || "Fill in variables"));

  const history = (await send(MSG.VAR_HISTORY_GET)).data || {};
  const snipHist = history[s.id] || {};

  const inputs = [];
  for (const name of vars) {
    const field = el("div", "sd-field");
    field.appendChild(el("label", null, name));
    const input = document.createElement("input");
    input.type = "text";
    input.dataset.var = name;
    const listId = `sd-hist-${name}`;
    const dl = document.createElement("datalist");
    dl.id = listId;
    for (const v of snipHist[name] || []) {
      const opt = document.createElement("option");
      opt.value = v;
      dl.appendChild(opt);
    }
    input.setAttribute("list", listId);
    field.appendChild(input);
    field.appendChild(dl);
    varsView.appendChild(field);
    inputs.push(input);
  }

  const actions = el("div", "sd-actions");
  const insertBtn = el("button", "primary", "Insert");
  const backBtn = el("button", null, "Back");
  actions.appendChild(insertBtn);
  actions.appendChild(backBtn);
  varsView.appendChild(actions);

  const doInsert = async () => {
    const values = {};
    for (const input of inputs) values[input.dataset.var] = input.value;
    insertIntoTarget(substitute(s.body || "", values));
    send(MSG.VAR_HISTORY_ADD, { snippetId: s.id, values });
    close();
  };
  insertBtn.addEventListener("click", doInsert);
  backBtn.addEventListener("click", () => backToList());
  for (const input of inputs) {
    input.addEventListener("keydown", (e) => {
      e.stopPropagation();
      if (e.key === "Enter") {
        e.preventDefault();
        doInsert();
      }
    });
  }
  inputs[0]?.focus();
}

function backToList() {
  mode = "list";
  pending = null;
  const panel = root.querySelector(".sd-panel");
  panel.querySelector(".sd-vars").classList.add("hidden");
  panel.querySelector(".sd-search").classList.remove("hidden");
  panel.querySelector(".sd-list").classList.remove("hidden");
  panel.querySelector(".sd-preview").classList.remove("hidden");
  panel.querySelector(".sd-search").focus();
}

async function open() {
  target = captureTarget();

  host = document.createElement("div");
  host.id = "snipdesk-overlay-host";
  host.style.cssText = "position:fixed;inset:0;z-index:2147483647;";
  root = host.attachShadow({ mode: "open" });

  const style = el("style");
  style.textContent = overlayCss;
  root.appendChild(style);

  const backdrop = el("div", "sd-backdrop");
  backdrop.innerHTML = `
    <div class="sd-panel" role="dialog" aria-label="SnipDesk launcher">
      <input class="sd-search" type="text" placeholder="Search snippets..." autocomplete="off" spellcheck="false" />
      <div class="sd-list" role="listbox"></div>
      <div class="sd-preview"></div>
      <div class="sd-vars hidden"></div>
      <div class="sd-hint"><kbd>up/down</kbd> move &nbsp; <kbd>Enter</kbd> insert &nbsp; <kbd>Esc</kbd> close</div>
    </div>`;
  root.appendChild(backdrop);
  document.documentElement.appendChild(host);

  backdrop.addEventListener("mousedown", (e) => {
    if (e.target === backdrop) close();
  });

  const search = root.querySelector(".sd-search");
  search.addEventListener("input", () => {
    applyFilter(search.value);
    renderList(search.value);
  });
  // Keep keystrokes from reaching the host page's own shortcuts while
  // the launcher has focus.
  search.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "ArrowDown") { e.preventDefault(); setSelected(selected + 1); }
    else if (e.key === "ArrowUp") { e.preventDefault(); setSelected(selected - 1); }
    else if (e.key === "Enter") { e.preventDefault(); choose(); }
  });

  settingsCache = (await send(MSG.SETTINGS_GET)).data || {};
  snippets = (await send(MSG.SNIPPETS_GET)).data || [];
  applyFilter("");
  renderList("");
  search.focus();
}

function toggle() {
  if (host) close();
  else open();
}

// Esc closes (or backs out of the variable prompt). Capture phase so
// the host page can't swallow it first.
document.addEventListener(
  "keydown",
  (e) => {
    if (e.key === "Escape" && host) {
      e.preventDefault();
      e.stopPropagation();
      if (mode === "vars") backToList();
      else close();
    }
  },
  true,
);

chrome.runtime.onMessage.addListener((msg) => {
  if (msg?.type === MSG.TOGGLE_LAUNCHER) toggle();
});
