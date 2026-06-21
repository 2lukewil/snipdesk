import { MSG, send } from "../shared/messages.js";
import { filterSnippets, sortSnippets } from "../shared/search.js";
import { extractVarNames, substitute, splitForPreview } from "../shared/variables.js";
import overlayCss from "./overlay.css?inline";

// Insertion target. `savedTarget` continuously tracks the last editable
// the user touched (so it survives the overlay stealing focus, or being
// opened from the toolbar); `target` is the one we insert into.
let target = null;
let savedTarget = null;

let host = null; // overlay host element
let root = null; // its shadow root
let snippets = [];
let filtered = [];
let selected = 0;
let mode = "list"; // "list" | "vars"
let pending = null; // snippet awaiting variable fill
let settingsCache = {};
let lastTextQuery = ""; // text part of the query, for highlighting

// A query may carry #tag tokens (AND-combined) alongside free text.
function parseQuery(raw) {
  const tags = [];
  const text = [];
  for (const term of raw.trim().split(/\s+/).filter(Boolean)) {
    if (term.startsWith("#") && term.length > 1) tags.push(term.slice(1).toLowerCase());
    else text.push(term);
  }
  return { text: text.join(" "), tags };
}

function applyFilter(rawQuery) {
  const { text, tags } = parseQuery(rawQuery);
  let list = text
    ? filterSnippets(snippets, text) // already rank-ordered
    : sortSnippets(snippets, settingsCache.sort_by_usage !== false);
  if (tags.length) {
    // Prefix match so a partial #tag narrows as you type.
    list = list.filter((s) => {
      const have = (s.tags || []).map((t) => t.toLowerCase());
      return tags.every((t) => have.some((h) => h.startsWith(t)));
    });
  }
  // Cap results so a large library doesn't build thousands of rows per
  // keystroke; ranked matches mean the top slice is what you want.
  filtered = list.length > 100 ? list.slice(0, 100) : list;
  selected = 0;
  lastTextQuery = text;
}

function computeTarget() {
  let el = document.activeElement;
  if (!el || el.id === "snipdesk-overlay-host") return null; // ignore our own overlay
  // Rich editors often host the editable inside a same-origin iframe;
  // descend into it. Cross-origin frames are unreachable and skipped.
  try {
    while (el && el.tagName === "IFRAME" && el.contentDocument) {
      el = el.contentDocument.activeElement;
    }
  } catch {
    /* cross-origin iframe: can't reach in */
  }
  if (!el) return null;
  const tag = el.tagName?.toLowerCase();
  if (tag === "input" || tag === "textarea") {
    return { el, kind: "input", start: el.selectionStart, end: el.selectionEnd };
  }
  if (el.isContentEditable) {
    const win = el.ownerDocument.defaultView || window;
    const sel = win.getSelection();
    let range = null;
    if (sel && sel.rangeCount && el.contains(sel.anchorNode)) {
      range = sel.getRangeAt(0).cloneRange();
    }
    return { el, kind: "ce", range };
  }
  return null;
}

// Keep the last editable target fresh as the user types/selects, so by
// the time the launcher opens (which moves focus to its own search box)
// we still know exactly where to insert and what was selected.
function trackSelection() {
  const t = computeTarget();
  if (t) savedTarget = t;
}
document.addEventListener("selectionchange", trackSelection, true);
document.addEventListener("focusin", trackSelection, true);
document.addEventListener("mouseup", trackSelection, true);
document.addEventListener("keyup", trackSelection, true);

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
  // contenteditable (possibly inside a same-origin iframe)
  const doc = el.ownerDocument;
  const win = doc.defaultView || window;
  el.focus({ preventScroll: true });
  const sel = win.getSelection();
  sel.removeAllRanges();
  if (target.range) sel.addRange(target.range);
  // No captured caret (e.g. selection was lost): drop one at the end.
  if (!sel.rangeCount) {
    const r = doc.createRange();
    r.selectNodeContents(el);
    r.collapse(false);
    sel.addRange(r);
  }
  let ok = false;
  try {
    ok = doc.execCommand("insertText", false, text);
  } catch {
    ok = false;
  }
  if (!ok && sel.rangeCount) {
    // Editors where execCommand is disabled: splice via the Range.
    const r = sel.getRangeAt(0);
    r.deleteContents();
    const node = doc.createTextNode(text);
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

// Inline autocomplete: when the typed text is a prefix of the top
// result's title, fill the rest and select it so the next keystroke
// overwrites it, or Tab/Right accepts. Skipped on delete and on #tag
// queries.
function maybeAutocomplete(e, input) {
  if (e.inputType && e.inputType.startsWith("delete")) return;
  const val = input.value;
  if (!val || val.includes("#")) return;
  const top = filtered[0];
  if (!top?.title) return;
  const title = top.title;
  if (title.length > val.length && title.toLowerCase().startsWith(val.toLowerCase())) {
    input.value = val + title.slice(val.length);
    input.setSelectionRange(val.length, input.value.length);
  }
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
    row.appendChild(titleNode(s.title || "(untitled)", lastTextQuery));
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

// Best-effort usage telemetry: bumps the local count (drives savings)
// and folds into the server totals. Fire-and-forget.
function reportUse(s, text) {
  const isLib = s.source === "library";
  const delta = { id: s.id, delta: 1, last_used: Math.floor(Date.now() / 1000) };
  send(MSG.USAGE_REPORT, {
    body: {
      chars_pasted_delta: (text || "").length,
      snippets_pasted_delta: 1,
      personal: isLib ? [] : [delta],
      library: isLib ? [delta] : [],
    },
  });
}

async function choose() {
  const s = filtered[selected];
  if (!s) return;
  const vars = extractVarNames(s.body || "");
  if (vars.length === 0) {
    insertIntoTarget(s.body || "");
    reportUse(s, s.body || "");
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
  panel.querySelector(".sd-body").classList.add("hidden");
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
    const rendered = substitute(s.body || "", values);
    insertIntoTarget(rendered);
    reportUse(s, rendered);
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
  panel.querySelector(".sd-body").classList.remove("hidden");
  panel.querySelector(".sd-search").focus();
}

async function open() {
  // Prefer a live read; fall back to the last tracked editable (e.g. if
  // focus already moved, or the launcher was opened from the toolbar).
  target = computeTarget() || savedTarget;

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
      <div class="sd-body">
        <div class="sd-list" role="listbox"></div>
        <div class="sd-preview"></div>
      </div>
      <div class="sd-vars hidden"></div>
      <div class="sd-hint"><kbd>up/down</kbd> move &nbsp; <kbd>Enter</kbd> insert &nbsp; <kbd>Esc</kbd> close</div>
    </div>`;
  root.appendChild(backdrop);
  document.documentElement.appendChild(host);

  backdrop.addEventListener("mousedown", (e) => {
    if (e.target === backdrop) close();
  });

  const search = root.querySelector(".sd-search");
  search.addEventListener("input", (e) => {
    applyFilter(search.value);
    renderList();
    maybeAutocomplete(e, search);
  });
  // Keep keystrokes from reaching the host page's own shortcuts while
  // the launcher has focus.
  search.addEventListener("keydown", (e) => {
    e.stopPropagation();
    const hasGhost =
      search.selectionStart !== search.selectionEnd &&
      search.selectionEnd === search.value.length;
    if (e.key === "ArrowDown") { e.preventDefault(); setSelected(selected + 1); }
    else if (e.key === "ArrowUp") { e.preventDefault(); setSelected(selected - 1); }
    else if (e.key === "Enter") { e.preventDefault(); choose(); }
    else if ((e.key === "Tab" || e.key === "ArrowRight") && hasGhost) {
      // Accept the ghost completion.
      e.preventDefault();
      search.setSelectionRange(search.value.length, search.value.length);
    }
  });

  settingsCache = (await send(MSG.SETTINGS_GET)).data || {};
  host.dataset.theme = settingsCache.theme === "light" ? "light" : "dark";
  snippets = (await send(MSG.SNIPPETS_GET)).data || [];
  applyFilter("");
  renderList();
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
