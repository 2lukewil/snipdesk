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

// Folder + tag navigation, mirroring the desktop launcher.
const ALL = "\u0000all";
const UNFILED = "\u0000unfiled";
const ICON_ALL = "\u{1F4D4}"; // notebook
const ICON_UNFILED = "\u{1F4C4}"; // page
const ICON_FOLDER = "\u{1F4C1}"; // folder
const CARET_OPEN = "▾"; // down-pointing
const CARET_CLOSED = "▸"; // right-pointing
const CLOUD = "☁"; // team marker
let selectedFolder = ALL;
let selectedTag = null; // lowercased tag, or null
let expanded = new Set();
let paneFocus = "list"; // "list" | "tree" - which section the arrows drive
let treeFocus = 0; // index into treeRows when paneFocus === "tree"
let treeRows = []; // [{ key, hasChildren, open, el }] in display order

function currentQuery() {
  return root?.querySelector(".sd-search")?.value || "";
}

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

function inFolder(s) {
  if (selectedFolder === ALL) return true;
  if (selectedFolder === UNFILED) return !s.folder_path;
  const fp = s.folder_path || "";
  return fp === selectedFolder || fp.startsWith(selectedFolder + "/");
}

function applyFilter(rawQuery) {
  const { text, tags } = parseQuery(rawQuery);
  // A text/#tag search or a selected tag spans every folder; the active
  // folder only scopes the plain browse view.
  const bypassFolder = text.length > 0 || tags.length > 0 || !!selectedTag;
  let list = snippets.slice();
  if (!bypassFolder && selectedFolder !== ALL) list = list.filter(inFolder);
  // Tag chip filter applies in both modes.
  if (selectedTag) {
    list = list.filter((s) => (s.tags || []).some((t) => t.toLowerCase() === selectedTag));
  }
  if (tags.length) {
    // Prefix match so a partial #tag narrows as you type.
    list = list.filter((s) => {
      const have = (s.tags || []).map((t) => t.toLowerCase());
      return tags.every((t) => have.some((h) => h.startsWith(t)));
    });
  }
  list = text
    ? filterSnippets(list, text) // already rank-ordered
    : sortSnippets(list, settingsCache.sort_by_usage !== false);
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

// Append text to a parent, wrapping every case-insensitive occurrence of
// the query in <mark>. DOM nodes only, never innerHTML.
function appendHighlighted(parent, text, query) {
  const needle = (query || "").trim().toLowerCase();
  if (!needle) {
    parent.appendChild(document.createTextNode(text));
    return;
  }
  const hay = text.toLowerCase();
  let pos = 0;
  let hit;
  while ((hit = hay.indexOf(needle, pos)) !== -1) {
    if (hit > pos) parent.appendChild(document.createTextNode(text.slice(pos, hit)));
    parent.appendChild(el("mark", null, text.slice(hit, hit + needle.length)));
    pos = hit + needle.length;
  }
  if (pos < text.length) parent.appendChild(document.createTextNode(text.slice(pos)));
}

function titleNode(title, query) {
  const wrap = el("div", "t");
  appendHighlighted(wrap, title, query);
  return wrap;
}

// ---- folder tree ----
function buildTree() {
  const rootNode = { children: new Map() };
  for (const s of snippets) {
    if (!s.folder_path) continue;
    let node = rootNode;
    let path = "";
    for (const part of s.folder_path.split("/").filter(Boolean)) {
      path = path ? `${path}/${part}` : part;
      if (!node.children.has(part)) {
        node.children.set(part, { name: part, path, children: new Map(), team: false });
      }
      node = node.children.get(part);
      if (s.source === "library") node.team = true;
    }
  }
  return rootNode;
}

function treeRow(label, icon, active, onClick, opts = {}) {
  const row = el("div", "sd-tnode" + (active ? " active" : ""));
  row.style.paddingLeft = `${8 + (opts.depth || 0) * 13}px`;
  const caret = el("span", "sd-caret");
  if (opts.hasChildren) {
    caret.textContent = opts.open ? CARET_OPEN : CARET_CLOSED;
    caret.addEventListener("mousedown", (e) => {
      e.preventDefault();
      e.stopPropagation();
      opts.onToggle();
    });
  }
  row.appendChild(caret);
  row.appendChild(el("span", "sd-ticon", icon));
  row.appendChild(el("span", "sd-tlabel", label));
  if (opts.team) row.appendChild(el("span", "sd-tcloud", CLOUD));
  row.addEventListener("mousedown", (e) => {
    e.preventDefault();
    onClick();
  });
  return row;
}

function renderTree() {
  const tree = root.querySelector(".sd-tree");
  if (!tree) return;
  tree.replaceChildren();
  treeRows = [];
  // While searching or tag-filtering, the folder scope is bypassed, so
  // dim the active-folder highlight to signal it isn't in effect.
  const bypassed = currentQuery().trim().length > 0 || !!selectedTag;
  const activeFolder = (f) => !bypassed && selectedFolder === f;
  const add = (key, rowEl, hasChildren, open) => {
    treeRows.push({ key, hasChildren: !!hasChildren, open: !!open, el: rowEl });
    tree.appendChild(rowEl);
  };
  add(ALL, treeRow("All snippets", ICON_ALL, activeFolder(ALL), () => selectFolder(ALL)), false, false);
  if (snippets.some((s) => !s.folder_path)) {
    add(UNFILED, treeRow("Unfiled", ICON_UNFILED, activeFolder(UNFILED), () => selectFolder(UNFILED)), false, false);
  }
  const walk = (node, depth) => {
    const children = [...node.children.values()].sort((a, b) => a.name.localeCompare(b.name));
    for (const child of children) {
      const hasChildren = child.children.size > 0;
      const open = expanded.has(child.path);
      const rowEl = treeRow(child.name, ICON_FOLDER, activeFolder(child.path), () => selectFolder(child.path), {
        depth,
        hasChildren,
        open,
        team: child.team,
        onToggle: () => {
          if (open) expanded.delete(child.path);
          else expanded.add(child.path);
          renderTree();
        },
      });
      add(child.path, rowEl, hasChildren, open);
      if (hasChildren && open) walk(child, depth + 1);
    }
  };
  walk(buildTree(), 0);
  if (paneFocus === "tree" && treeRows.length) {
    treeFocus = Math.min(treeRows.length - 1, Math.max(0, treeFocus));
    treeRows[treeFocus].el.classList.add("kbd");
  }
}

function selectFolder(f) {
  selectedFolder = f;
  renderTree();
  applyFilter(currentQuery());
  renderList();
  root.querySelector(".sd-search")?.focus();
}

// ---- keyboard navigation between the tree and list sections ----
function enterTree() {
  paneFocus = "tree";
  const idx = treeRows.findIndex((r) => r.key === selectedFolder);
  treeFocus = idx >= 0 ? idx : 0;
  renderTree();
  treeRows[treeFocus]?.el.scrollIntoView({ block: "nearest" });
}

function moveTreeFocus(delta) {
  if (!treeRows.length) return;
  treeFocus = Math.min(treeRows.length - 1, Math.max(0, treeFocus + delta));
  renderTree();
  treeRows[treeFocus]?.el.scrollIntoView({ block: "nearest" });
}

// Apply the focused folder and return focus to the snippet list.
function applyTreeFocus() {
  const r = treeRows[treeFocus];
  paneFocus = "list";
  if (r) selectFolder(r.key);
}

// ---- tag strip ----
function renderTagStrip() {
  const strip = root.querySelector(".sd-tags");
  if (!strip) return;
  const tags = [...new Set(snippets.flatMap((s) => s.tags || []))].sort((a, b) => a.localeCompare(b));
  strip.replaceChildren();
  strip.classList.toggle("hidden", tags.length === 0);
  if (selectedTag && !tags.some((t) => t.toLowerCase() === selectedTag)) selectedTag = null;
  for (const tag of tags) {
    const lc = tag.toLowerCase();
    const chip = el("button", "sd-chip" + (selectedTag === lc ? " active" : ""), tag);
    chip.addEventListener("mousedown", (e) => {
      e.preventDefault();
      selectedTag = selectedTag === lc ? null : lc;
      renderTagStrip();
      applyFilter(currentQuery());
      renderList();
      root.querySelector(".sd-search")?.focus();
    });
    strip.appendChild(chip);
  }
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
    if (s.body) {
      const bodyEl = el("div", "sd-rowbody");
      appendHighlighted(bodyEl, s.body.replace(/\s+/g, " ").slice(0, 120), lastTextQuery);
      row.appendChild(bodyEl);
    }
    const meta = el("div", "meta");
    if (s.source === "library") {
      meta.appendChild(el("span", "sd-rowcloud", CLOUD));
    }
    if (s.folder_path) {
      const folder = el("span", "sd-rowfolder");
      folder.appendChild(el("span", "sd-rowfolder-icon", ICON_FOLDER));
      folder.appendChild(document.createTextNode(s.folder_path));
      meta.appendChild(folder);
    }
    for (const tag of s.tags || []) meta.appendChild(el("span", "sd-rowtag", tag));
    if (meta.childNodes.length) row.appendChild(meta);
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
      appendHighlighted(preview, chunk.text, lastTextQuery);
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
      <div class="sd-tags hidden"></div>
      <div class="sd-body">
        <div class="sd-tree"></div>
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
    paneFocus = "list"; // typing always returns to the search/list section
    applyFilter(search.value);
    renderTree(); // active-folder highlight dims while searching
    renderList();
    maybeAutocomplete(e, search);
  });
  // Keep keystrokes from reaching the host page's own shortcuts while
  // the launcher has focus. Arrows move within the focused section; Left
  // jumps to the folder tree, Right/Enter come back to the list.
  search.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (paneFocus === "tree") {
      const r = treeRows[treeFocus];
      if (e.key === "ArrowDown") { e.preventDefault(); moveTreeFocus(1); }
      else if (e.key === "ArrowUp") { e.preventDefault(); moveTreeFocus(-1); }
      else if (e.key === "Enter") { e.preventDefault(); applyTreeFocus(); }
      else if (e.key === "ArrowRight") {
        e.preventDefault();
        if (r?.hasChildren && !r.open) { expanded.add(r.key); renderTree(); }
        else applyTreeFocus();
      } else if (e.key === "ArrowLeft") {
        e.preventDefault();
        if (r?.hasChildren && r.open) { expanded.delete(r.key); renderTree(); }
        else { paneFocus = "list"; renderTree(); }
      }
      return;
    }
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
    } else if (e.key === "ArrowLeft" && search.selectionStart === 0 && search.selectionEnd === 0) {
      // At the start of the query: step into the folder tree.
      e.preventDefault();
      enterTree();
    }
  });

  settingsCache = (await send(MSG.SETTINGS_GET)).data || {};
  host.dataset.theme = settingsCache.theme === "light" ? "light" : "dark";
  snippets = (await send(MSG.SNIPPETS_GET)).data || [];
  paneFocus = "list";
  renderTree();
  renderTagStrip();
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
