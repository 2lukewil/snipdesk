import { MSG, send } from "../shared/messages.js";
import { filterSnippets, sortSnippets } from "../shared/search.js";
import { validateSnippet } from "../shared/validate.js";
import { splitForPreview } from "../shared/variables.js";
import { renderFormatted } from "../shared/format.js";
import {
  getPendingFolders,
  setPendingFolders,
  getFolderOrder,
  setFolderOrder,
  getPendingNewSnippet,
  clearPendingNewSnippet,
  DEFAULT_FORMAT_RULES,
} from "../shared/storage.js";

const $ = (id) => document.getElementById(id);
const el = (tag, cls, text) => {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text != null) n.textContent = text;
  return n;
};

// Transient bottom notification. Used to surface server-op failures so
// they don't fail silently (e.g. when the server is unreachable).
function toast(message, isError) {
  const t = el("div", "toast" + (isError ? " err" : ""), message);
  document.body.appendChild(t);
  requestAnimationFrame(() => t.classList.add("show"));
  setTimeout(() => {
    t.classList.remove("show");
    setTimeout(() => t.remove(), 200);
  }, 3200);
}

const SERVER_ERR = "Server request failed. Is the server reachable?";

const ALL = "__all__";
const UNFILED = "__unfiled__";
const MAX_LIST_ROWS = 500; // cap rendered rows so huge libraries stay responsive

// Glyphs mirror the desktop folder tree.
const ICON_ALL = "\u{1F4D4}"; // notebook with decorative cover
const ICON_UNFILED = "\u{1F4C4}"; // page facing up
const ICON_FOLDER = "\u{1F4C1}"; // file folder
const CARET_OPEN = "▾"; // down-pointing triangle
const CARET_CLOSED = "▸"; // right-pointing triangle

// Monochrome pencil (inherits currentColor, so it tints on hover like
// the delete x) instead of a color emoji.
const PENCIL_SVG =
  '<svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M17 3a2.828 2.828 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z"/></svg>';

let settings = {};
let editingRules = []; // working copy of format_rules, flushed on Save
let snippets = [];
let pendingFolders = [];
let folderOrder = {};
let selectedId = null;
let selectedIds = new Set();
let anchorIndex = null;
let selectedTag = null;
let visibleItems = [];
let selectedFolder = ALL;
let draggingFolderPath = null;
const expanded = new Set();
let treeKeys = []; // [{ key, hasKids }] in display order, for arrow-key nav
let navPane = "list"; // "list" | "tree" - which section the arrow keys drive

async function init() {
  const status = (await send(MSG.AUTH_STATUS)).data || {};
  $("dock").classList.remove("hidden");
  $("tab-snippets").classList.remove("hidden");
  if (status.signedIn) {
    $("btn-logout").classList.remove("hidden");
    $("identity").textContent = status.user?.display_name || status.user?.email || "";
    if (status.user?.role === "admin") {
      $("btn-dashboard").classList.remove("hidden");
      $("admin-badge").classList.remove("hidden");
    }
  }
  // Signed out is local-only: the bottom-left status dot reads "Local only";
  // the toolbar and dock stay fully usable.

  settings = (await send(MSG.SETTINGS_GET)).data || {};
  applyTheme();
  pendingFolders = await getPendingFolders();
  folderOrder = await getFolderOrder();
  await loadSnippets();
  fillSettingsForm();
  applyDensity();
  wire();
  $("server-status").classList.remove("hidden");
  statusTick();
  setInterval(statusTick, 12000);
  window.addEventListener("offline", refreshServerStatus);
  window.addEventListener("online", statusTick);

  if (!settings.onboarded) showOnboarding();

  // Text captured from a page's right-click menu opens a prefilled
  // new snippet.
  const pendingNew = await getPendingNewSnippet();
  if (pendingNew) {
    await clearPendingNewSnippet();
    openEditor(null, { body: pendingNew });
  }
}

async function loadSnippets() {
  snippets = (await send(MSG.SNIPPETS_GET)).data || [];
  renderTree();
  renderTagStrip();
  renderList();
  refreshServerStatus();
}

// Probe the server (when signed in and online) so "Disconnected" reflects a
// server that went down, then repaint the dot. Signed-out is local-only with
// no server to reach, so we skip the sync entirely.
async function statusTick() {
  const st = (await send(MSG.AUTH_STATUS)).data || {};
  if (st.signedIn && navigator.onLine) await send(MSG.SYNC_NOW);
  refreshServerStatus();
}

// Live connection state in the bottom-left dot: neutral when local-only
// (not signed in), red when offline or the last sync failed, amber while
// local writes are still flushing, green when connected and fully synced.
async function refreshServerStatus() {
  const box = $("server-status");
  if (!box) return;
  const st = (await send(MSG.AUTH_STATUS)).data || {};
  if (!st.signedIn) {
    box.className = "server-status local";
    box.querySelector(".status-text").textContent = "Local only";
    return;
  }
  const pending = st.pending || 0;
  const offline = !navigator.onLine;
  const failed = st.lastSync && !st.lastSync.ok;
  let cls, label;
  if (offline || failed) {
    cls = "err";
    label = offline ? "Offline" : "Disconnected";
  } else if (pending) {
    cls = "pending";
    label = `Syncing ${pending}...`;
  } else {
    cls = "ok";
    label = "Connected";
  }
  box.className = `server-status ${cls}`;
  box.querySelector(".status-text").textContent = label;
}

// ---- tag filter strip ----
function renderTagStrip() {
  const strip = $("tag-strip");
  const tags = [...new Set(snippets.flatMap((s) => s.tags || []))].sort((a, b) => a.localeCompare(b));
  strip.replaceChildren();
  strip.classList.toggle("hidden", tags.length === 0);
  if (selectedTag && !tags.includes(selectedTag)) selectedTag = null;
  for (const tag of tags) {
    const chip = el("button", "tag-chip" + (selectedTag === tag ? " active" : ""), tag);
    chip.addEventListener("click", () => {
      selectedTag = selectedTag === tag ? null : tag;
      renderTagStrip();
      renderList();
    });
    strip.appendChild(chip);
  }
}

// ---- modals (trash / settings) ----
function openModalEl(id) {
  $(id).classList.remove("hidden");
  if (id === "settings-modal") fillSettingsForm();
  if (id === "trash-modal") loadTrash();
}

function closeModalEl(id) {
  $(id).classList.add("hidden");
}

function closeAllModals() {
  closeModalEl("settings-modal");
  closeModalEl("trash-modal");
}

// ---- first-run tour ----
const OB_STEPS = ["welcome", "launcher", "organize", "typing", "wage", "team", "done"];
const OB_PHRASE =
  "A time may come soon, when none will return. Then there will be need of valour without renown, " +
  "for none shall remember the deeds that are done in the last defence of your homes. " +
  "Yet the deeds will not be less valiant because they are unpraised.";
let obStep = 0;
let obTyping = null; // { startedAt, wpm }
let obWpm = null; // measured WPM, applied on advancing past the typing step

function guessCurrencyFromLocale() {
  const map = {
    AU: "AUD", US: "USD", GB: "GBP", DE: "EUR", FR: "EUR", IT: "EUR", ES: "EUR",
    NL: "EUR", AT: "EUR", BE: "EUR", IE: "EUR", PT: "EUR", FI: "EUR", GR: "EUR",
    JP: "JPY", CA: "CAD", NZ: "NZD", CH: "CHF", IN: "INR", SG: "SGD", HK: "HKD",
    ZA: "ZAR", BR: "BRL", MX: "MXN", KR: "KRW", SE: "SEK", NO: "NOK", DK: "DKK",
    PL: "PLN", CZ: "CZK", TR: "TRY", AE: "AED", CN: "CNY", TH: "THB", ID: "IDR",
    PH: "PHP",
  };
  const region = (navigator.language || "").toUpperCase().split(/[-_]/)[1] || "";
  return map[region] || "AUD"; // fall back to a valid dropdown code
}

function primeTyping() {
  const phrase = $("ob-typing-phrase");
  const input = $("ob-typing-input");
  phrase.replaceChildren();
  for (const ch of OB_PHRASE) {
    const span = el("span", "ch", ch);
    phrase.appendChild(span);
  }
  phrase.querySelector(".ch")?.classList.add("cursor");
  phrase.onclick = () => input.focus();
  input.onfocus = () => phrase.classList.add("is-focused");
  input.onblur = () => phrase.classList.remove("is-focused");
  input.value = "";
  obTyping = { startedAt: null };
  obWpm = null;
  $("ob-typing-result").classList.add("hidden");
  $("ob-typing-restart").classList.add("hidden");
  setTimeout(() => input.focus(), 50);
}

function onTypingInput() {
  const phrase = $("ob-typing-phrase");
  const typed = $("ob-typing-input").value;
  if (!obTyping) return;
  if (typed.length > 0 && obTyping.startedAt === null) obTyping.startedAt = Date.now();
  const spans = phrase.querySelectorAll(".ch");
  spans.forEach((s, i) => {
    s.classList.remove("ok", "bad", "cursor");
    if (i < typed.length) s.classList.add(typed[i] === OB_PHRASE[i] ? "ok" : "bad");
    else if (i === typed.length) s.classList.add("cursor");
  });
  if (typed === OB_PHRASE && obWpm === null) {
    const seconds = (Date.now() - obTyping.startedAt) / 1000;
    const words = OB_PHRASE.trim().split(/\s+/).length;
    obWpm = Math.max(1, Math.round((words * 60) / seconds));
    const result = $("ob-typing-result");
    result.replaceChildren(el("strong", null, `${obWpm} WPM`), document.createTextNode(` over ${seconds.toFixed(1)}s.`));
    result.classList.remove("hidden");
    $("ob-typing-restart").classList.remove("hidden");
    $("ob-typing-input").blur();
  }
}

function primeWage() {
  const sel = $("ob-currency");
  if (sel.options.length === 0) {
    const codes = ["AUD","USD","EUR","GBP","CAD","NZD","JPY","CHF","INR","SGD","HKD","ZAR","BRL","MXN","KRW","SEK","NOK","DKK","PLN","CZK","TRY","AED","CNY","THB","IDR","PHP"];
    for (const c of codes) sel.appendChild(el("option", null, c)).value = c;
  }
  $("ob-wage").value = settings.hourly_wage > 0 ? String(settings.hourly_wage) : "";
  const existing = settings.wage_currency;
  sel.value = [...sel.options].some((o) => o.value === existing) ? existing : guessCurrencyFromLocale();
}

// Persist whatever the actionable step (typing / wage) gathered before
// moving on. Next never forces the task: an untyped test or blank wage
// just leaves those settings untouched.
async function commitOnboardingStep(step) {
  let patch = null;
  if (step === "typing" && obWpm !== null) patch = { typing_speed_wpm: obWpm };
  if (step === "wage") {
    const wage = parseFloat($("ob-wage").value);
    patch = { wage_currency: $("ob-currency").value || "$" };
    if (Number.isFinite(wage) && wage > 0) {
      patch.hourly_wage = wage;
      patch.show_savings_estimate = true; // they set a wage; surface the estimate
    }
  }
  if (patch) {
    const res = await send(MSG.SETTINGS_SET, { patch });
    if (res.ok) settings = res.data;
  }
}

function renderOnboarding() {
  for (const panel of document.querySelectorAll("#onboarding-modal .ob-panel")) {
    panel.classList.toggle("hidden", panel.dataset.step !== OB_STEPS[obStep]);
  }
  const prog = $("ob-progress");
  prog.replaceChildren();
  OB_STEPS.forEach((_, i) => prog.appendChild(el("span", "ob-dot" + (i === obStep ? " active" : ""))));
  $("ob-back").classList.toggle("hidden", obStep === 0);
  $("ob-next").textContent = obStep === OB_STEPS.length - 1 ? "Get started" : "Next";
  if (OB_STEPS[obStep] === "typing") primeTyping();
  if (OB_STEPS[obStep] === "wage") primeWage();
}

function showOnboarding() {
  obStep = 0;
  renderOnboarding();
  $("onboarding-modal").classList.remove("hidden");
}

async function finishOnboarding() {
  await commitOnboardingStep(OB_STEPS[obStep]);
  $("onboarding-modal").classList.add("hidden");
  const res = await send(MSG.SETTINGS_SET, { patch: { onboarded: true } });
  if (res.ok) settings = res.data;
}

// ---- folder tree ----
function ensurePath(root, fullPath) {
  let node = root;
  let path = "";
  const parts = [];
  for (const part of fullPath.split("/").filter(Boolean)) {
    path = path ? `${path}/${part}` : part;
    if (!node.children.has(part)) {
      node.children.set(part, { name: part, path, children: new Map(), count: 0 });
    }
    node = node.children.get(part);
    parts.push(node);
  }
  return parts;
}

function buildTree() {
  const root = { children: new Map() };
  for (const s of snippets) {
    if (!s.folder_path) continue;
    for (const node of ensurePath(root, s.folder_path)) {
      node.count += 1; // snippets in this folder or any descendant
    }
  }
  // Empty folders the user created carry no snippets, so they only
  // appear by unioning them in (count stays 0).
  for (const fp of pendingFolders) ensurePath(root, fp);
  return root;
}

// Siblings sort by manual order where set, alphabetical otherwise.
function sortedChildren(node) {
  return [...node.children.values()].sort((a, b) => {
    const oa = folderOrder[a.path];
    const ob = folderOrder[b.path];
    if (oa != null && ob != null) return oa - ob;
    if (oa != null) return -1;
    if (ob != null) return 1;
    return a.name.localeCompare(b.name);
  });
}

function parentOf(path) {
  const i = path.lastIndexOf("/");
  return i < 0 ? "" : path.slice(0, i);
}

function nodeAtPath(root, path) {
  if (!path) return root;
  let node = root;
  for (const part of path.split("/")) {
    node = node.children?.get(part);
    if (!node) return null;
  }
  return node;
}

function renderTree() {
  const tree = $("tree");
  tree.replaceChildren();
  treeKeys = [];
  const unfiledCount = snippets.filter((s) => !s.folder_path).length;

  tree.appendChild(folderNode({ label: "All snippets", key: ALL, count: snippets.length, depth: 0, hasKids: false, icon: ICON_ALL }));
  treeKeys.push({ key: ALL, hasKids: false });
  tree.appendChild(folderNode({ label: "Unfiled", key: UNFILED, count: unfiledCount, depth: 0, hasKids: false, icon: ICON_UNFILED }));
  treeKeys.push({ key: UNFILED, hasKids: false });
  tree.appendChild(rootDropZone());

  const root = buildTree();
  const walk = (node, depth) => {
    for (const child of sortedChildren(node)) {
      const hasKids = child.children.size > 0;
      tree.appendChild(
        folderNode({ label: child.name, key: child.path, count: child.count, depth, hasKids, icon: ICON_FOLDER, real: true }),
      );
      treeKeys.push({ key: child.path, hasKids });
      if (hasKids && expanded.has(child.path)) walk(child, depth + 1);
    }
  };
  walk(root, 0);
  applyNavHighlight();
}

// The active folder is blue while the folder tree is the section the
// arrows drive, and plain grey when it's just the open folder/scope.
function applyNavHighlight() {
  $("tree").classList.toggle("nav-active", navPane === "tree");
}

// Move the active folder (filtering the list live). The active highlight
// is the only cue: a highlighted folder + no highlighted snippet means
// folder navigation is in effect, and vice versa.
function selectFolderKey(key) {
  selectedFolder = key;
  if (selectedIds.size > 1) {
    selectedIds = new Set();
    clearEditor();
  }
  renderTree();
  renderList();
  $("tree").querySelector(".tree-node.active")?.scrollIntoView({ block: "nearest" });
}

function moveTreeSel(dir) {
  if (!treeKeys.length) return;
  let idx = treeKeys.findIndex((k) => k.key === selectedFolder);
  if (idx < 0) idx = 0;
  idx = dir === "down" ? Math.min(treeKeys.length - 1, idx + 1) : Math.max(0, idx - 1);
  selectFolderKey(treeKeys[idx].key);
}

function toggleFolderExpand(key) {
  const node = treeKeys.find((k) => k.key === key);
  if (!node?.hasKids) return;
  if (expanded.has(key)) expanded.delete(key);
  else expanded.add(key);
  renderTree();
  $("tree").querySelector(".tree-node.active")?.scrollIntoView({ block: "nearest" });
}

// Drop a nested folder here to lift it back to the top level.
function rootDropZone() {
  const zone = el("div", "tree-root-drop", "Drop here to move to top level");
  zone.addEventListener("dragover", (e) => {
    if (!e.dataTransfer.types.includes("text/folder-path")) return;
    e.preventDefault();
    zone.classList.add("drop-target");
  });
  zone.addEventListener("dragleave", () => zone.classList.remove("drop-target"));
  zone.addEventListener("drop", (e) => {
    e.preventDefault();
    zone.classList.remove("drop-target");
    const folderPath = e.dataTransfer.getData("text/folder-path");
    if (folderPath) moveFolderInto(folderPath, null);
  });
  return zone;
}

function folderNode({ label, key, count, depth, hasKids, icon, real }) {
  const node = el("div", "tree-node");
  node.classList.toggle("active", selectedFolder === key);
  node.style.paddingLeft = `${10 + depth * 12}px`;

  const caret = el("span", "tree-caret" + (hasKids ? "" : " leaf"), hasKids ? (expanded.has(key) ? CARET_OPEN : CARET_CLOSED) : "");
  if (hasKids) {
    caret.addEventListener("click", (e) => {
      e.stopPropagation();
      if (expanded.has(key)) expanded.delete(key);
      else expanded.add(key);
      renderTree();
    });
  }
  node.appendChild(caret);
  node.appendChild(el("span", "tree-icon", icon));
  node.appendChild(el("span", "tree-label", label));
  if (count) node.appendChild(el("span", "tree-count", String(count)));

  // Hover-revealed rename and delete on real folders.
  if (real) {
    const rename = el("button", "tree-edit");
    rename.innerHTML = PENCIL_SVG;
    rename.title = "Rename folder";
    rename.addEventListener("click", (e) => {
      e.stopPropagation();
      renameFolder(key);
    });
    node.appendChild(rename);

    const del = el("button", "tree-del", "×");
    del.title = count ? "Delete folder" : "Delete empty folder";
    del.addEventListener("click", (e) => {
      e.stopPropagation();
      requestFolderDelete(key);
    });
    node.appendChild(del);
  }

  // Real folders can be dragged: onto another folder to nest, onto a
  // sibling's top/bottom edge to reorder, onto the root zone to unnest.
  if (real) {
    node.draggable = true;
    node.addEventListener("dragstart", (e) => {
      e.stopPropagation();
      draggingFolderPath = key;
      e.dataTransfer.setData("text/folder-path", key);
      e.dataTransfer.effectAllowed = "move";
      node.classList.add("dragging");
    });
    node.addEventListener("dragend", () => {
      draggingFolderPath = null;
      node.classList.remove("dragging");
    });
  }

  node.addEventListener("click", () => {
    navPane = "tree";
    selectedFolder = key;
    if (selectedIds.size > 1) {
      selectedIds = new Set();
      clearEditor();
    }
    renderTree();
    renderList();
  });

  const clearDropClasses = () =>
    node.classList.remove("drop-target", "drop-above", "drop-below");

  node.addEventListener("dragover", (e) => {
    e.preventDefault();
    clearDropClasses();
    const isFolder = e.dataTransfer.types.includes("text/folder-path");
    if (isFolder && real) {
      const pos = folderDropPosition(node, key, e.clientY);
      node.classList.add(pos === "above" ? "drop-above" : pos === "below" ? "drop-below" : "drop-target");
    } else {
      node.classList.add("drop-target");
    }
  });
  node.addEventListener("dragleave", clearDropClasses);
  node.addEventListener("drop", (e) => {
    e.preventDefault();
    const pos = real ? folderDropPosition(node, key, e.clientY) : "into";
    clearDropClasses();
    const snippetId = e.dataTransfer.getData("text/snippet-id");
    const folderPath = e.dataTransfer.getData("text/folder-path");
    if (snippetId) {
      const target = key === ALL || key === UNFILED ? null : key;
      // Dragging one row of a multi-selection moves the whole set.
      if (selectedIds.has(snippetId) && selectedIds.size > 1) {
        const personal = [...selectedIds].filter((id) =>
          snippets.some((s) => s.id === id && s.source === "personal"),
        );
        bulkMove(personal, target);
      } else {
        moveSnippetToFolder(snippetId, target);
      }
    } else if (folderPath) {
      if (key === UNFILED) return; // Unfiled isn't a folder target
      if (key === ALL) { moveFolderInto(folderPath, null); return; } // unnest
      if (pos === "above" || pos === "below") reorderFolder(folderPath, key, pos);
      else moveFolderInto(folderPath, key);
    }
  });
  return node;
}

// Above/below = reorder among same-parent siblings; otherwise nest.
function folderDropPosition(nodeEl, key, clientY) {
  if (!draggingFolderPath || draggingFolderPath === key) return "into";
  if (parentOf(draggingFolderPath) !== parentOf(key)) return "into";
  const r = nodeEl.getBoundingClientRect();
  const ratio = (clientY - r.top) / Math.max(r.height, 1);
  if (ratio < 0.3) return "above";
  if (ratio > 0.7) return "below";
  return "into";
}

async function reorderFolder(srcPath, targetPath, position) {
  if (parentOf(srcPath) !== parentOf(targetPath)) return;
  const parentNode = nodeAtPath(buildTree(), parentOf(srcPath));
  if (!parentNode) return;
  const order = sortedChildren(parentNode)
    .map((c) => c.path)
    .filter((p) => p !== srcPath);
  let idx = order.indexOf(targetPath);
  if (idx < 0) return;
  if (position === "below") idx += 1;
  order.splice(idx, 0, srcPath);
  order.forEach((p, i) => {
    folderOrder[p] = i;
  });
  await setFolderOrder(folderOrder);
  renderTree();
}

async function moveSnippetToFolder(id, folderPath) {
  const s = snippets.find((x) => x.id === id);
  if (!s || s.source !== "personal") return;
  if ((s.folder_path || null) === (folderPath || null)) return;
  const payload = {
    title: s.title,
    body: s.body,
    tags: s.tags || [],
    folder_path: folderPath || null,
  };
  const res = await send(MSG.SNIPPET_UPDATE, { id, expectedVersion: s.version, payload });
  if (res.ok) await loadSnippets();
  else toast(res.error || SERVER_ERR, true);
}

// Personal folders are derived from folder_path strings, so re-nesting
// or renaming a folder means rewriting the prefix on every snippet at
// or under it (plus the local pending-folder and order maps).
function descendantsOf(srcPath) {
  return snippets.filter(
    (s) =>
      s.source === "personal" &&
      s.folder_path &&
      (s.folder_path === srcPath || s.folder_path.startsWith(srcPath + "/")),
  );
}

// Team-library snippets are read-only (managed in the dashboard), so a
// folder that contains them can't be fully moved/renamed from here.
function teamUnder(path) {
  return snippets.filter(
    (s) =>
      s.source === "library" &&
      s.folder_path &&
      (s.folder_path === path || s.folder_path.startsWith(path + "/")),
  ).length;
}

async function rewriteFolderPrefix(srcPath, newBase) {
  if (!newBase || newBase === srcPath) return;
  if ((newBase + "/").startsWith(srcPath + "/")) return; // into own descendant
  const teamN = teamUnder(srcPath);
  for (const s of descendantsOf(srcPath)) {
    const next = newBase + s.folder_path.slice(srcPath.length);
    const payload = { title: s.title, body: s.body, tags: s.tags || [], folder_path: next };
    const res = await send(MSG.SNIPPET_UPDATE, { id: s.id, expectedVersion: s.version, payload });
    if (!res.ok) {
      toast(res.error || SERVER_ERR, true);
      break;
    }
  }
  const remap = (p) => (p === srcPath || p.startsWith(srcPath + "/") ? newBase + p.slice(srcPath.length) : p);
  pendingFolders = [...new Set(pendingFolders.map(remap))];
  await setPendingFolders(pendingFolders);
  const nextOrder = {};
  for (const [p, i] of Object.entries(folderOrder)) nextOrder[remap(p)] = i;
  folderOrder = nextOrder;
  await setFolderOrder(folderOrder);
  // Carry expanded state across the rename so descendants stay open.
  const wasExpanded = [...expanded];
  expanded.clear();
  for (const p of wasExpanded) expanded.add(remap(p));
  expandAncestors(newBase);
  selectedFolder = newBase;
  await loadSnippets();
  if (teamN) {
    toast(`${teamN} team snippet${teamN > 1 ? "s" : ""} stay in "${srcPath}" (managed in the dashboard).`);
  }
}

function moveFolderInto(srcPath, targetFolder) {
  if (targetFolder === srcPath) return;
  const leaf = srcPath.split("/").pop();
  const newBase = targetFolder ? `${targetFolder}/${leaf}` : leaf;
  return rewriteFolderPrefix(srcPath, newBase);
}

async function renameFolder(srcPath) {
  const leaf = srcPath.split("/").pop();
  const name = prompt("Rename folder", leaf);
  if (name == null) return;
  const clean = name.trim().replace(/\//g, "");
  if (!clean || clean === leaf) return;
  const parent = parentOf(srcPath);
  await rewriteFolderPrefix(srcPath, parent ? `${parent}/${clean}` : clean);
}

async function createFolder() {
  const input = $("folder-create-input");
  const raw = input.value.replace(/^[\s/]+|[\s/]+$/g, "");
  if (!raw || raw.length > 300 || raw.includes("//")) return;
  if (!folderExists(raw) && !pendingFolders.includes(raw)) {
    pendingFolders.push(raw);
    await setPendingFolders(pendingFolders);
  }
  input.value = "";
  expandAncestors(raw);
  selectedFolder = raw;
  renderTree();
  renderList();
}

function folderExists(path) {
  return snippets.some(
    (s) => s.folder_path && (s.folder_path === path || s.folder_path.startsWith(path + "/")),
  );
}

// Generic confirm/choice modal.
function openModal(title, message, buttons) {
  const back = el("div", "modal-back");
  const box = el("div", "modal");
  box.appendChild(el("h2", null, title));
  if (message) box.appendChild(el("p", "modal-msg", message));
  const row = el("div", "modal-actions");
  for (const b of buttons) {
    const btn = el("button", b.cls || "", b.label);
    btn.addEventListener("click", () => {
      back.remove();
      b.onClick?.();
    });
    row.appendChild(btn);
  }
  box.appendChild(row);
  back.appendChild(box);
  back.addEventListener("mousedown", (e) => {
    if (e.target === back) back.remove();
  });
  document.body.appendChild(back);
}

// Empty folder: drop it. Populated folder: ask whether to keep the
// snippets (move to Unfiled) or trash them along with the folder.
function requestFolderDelete(path) {
  const count = descendantsOf(path).length;
  if (count === 0) {
    if (teamUnder(path)) {
      toast("This folder holds only team snippets (managed in the dashboard).");
      return;
    }
    pendingFolders = pendingFolders.filter((p) => p !== path && !p.startsWith(path + "/"));
    setPendingFolders(pendingFolders).then(() => {
      if (selectedFolder === path) selectedFolder = ALL;
      renderTree();
      renderList();
    });
    return;
  }
  openModal(
    `Delete folder "${path}"?`,
    `${count} snippet${count > 1 ? "s are" : " is"} in this folder.`,
    [
      { label: "Move to Unfiled", cls: "primary", onClick: () => deleteFolder(path, "move") },
      { label: "Delete folder and snippets", cls: "danger", onClick: () => deleteFolder(path, "delete") },
      { label: "Cancel" },
    ],
  );
}

async function deleteFolder(path, mode) {
  for (const s of descendantsOf(path)) {
    let res;
    if (mode === "delete") {
      res = await send(MSG.SNIPPET_DELETE, { id: s.id });
    } else {
      const payload = { title: s.title, body: s.body, tags: s.tags || [], folder_path: null };
      res = await send(MSG.SNIPPET_UPDATE, { id: s.id, expectedVersion: s.version, payload });
    }
    if (!res.ok) {
      toast(res.error || SERVER_ERR, true);
      break;
    }
  }
  pendingFolders = pendingFolders.filter((p) => p !== path && !p.startsWith(path + "/"));
  await setPendingFolders(pendingFolders);
  if (selectedFolder === path || selectedFolder.startsWith(path + "/")) selectedFolder = ALL;
  await loadSnippets();
  clearEditor();
}

function expandAncestors(path) {
  const parts = path.split("/");
  for (let i = 1; i <= parts.length; i++) expanded.add(parts.slice(0, i).join("/"));
}

// ---- snippet list ----
function inFolder(s) {
  if (selectedFolder === ALL) return true;
  if (selectedFolder === UNFILED) return !s.folder_path;
  const fp = s.folder_path || "";
  return fp === selectedFolder || fp.startsWith(selectedFolder + "/");
}

function visibleSnippets() {
  const q = $("search").value;
  let list = filterSnippets(snippets, q);
  // A selected tag spans every folder; the active folder only scopes the
  // plain browse view.
  if (selectedTag) list = list.filter((s) => (s.tags || []).includes(selectedTag));
  else list = list.filter(inFolder);
  if (!q.trim()) list = sortSnippets(list, settings.sort_by_usage !== false);
  return list;
}

// Append text to `parent`, wrapping case-insensitive query matches in
// <mark>. `q` is expected already lowercased.
function appendHighlighted(parent, text, q) {
  if (!q) {
    parent.appendChild(document.createTextNode(text));
    return;
  }
  const lower = text.toLowerCase();
  let from = 0;
  let idx;
  while ((idx = lower.indexOf(q, from)) >= 0) {
    if (idx > from) parent.appendChild(document.createTextNode(text.slice(from, idx)));
    parent.appendChild(el("mark", null, text.slice(idx, idx + q.length)));
    from = idx + q.length;
  }
  if (from < text.length) parent.appendChild(document.createTextNode(text.slice(from)));
}

// One-line body sample: variables in amber, query matches highlighted.
function renderBodySample(parent, body, q) {
  const sample = (body || "").replace(/\s+/g, " ").trim().slice(0, 160);
  for (const chunk of splitForPreview(sample)) {
    if (chunk.type === "text") appendHighlighted(parent, chunk.text, q);
    else parent.appendChild(el("var", null, `{${chunk.name}}`));
  }
}

function renderList() {
  const list = $("list");
  list.replaceChildren();
  const q = $("search").value.trim().toLowerCase();
  const all = visibleSnippets();
  if (all.length === 0) {
    const blank = snippets.length === 0 && !q && selectedFolder === ALL && !selectedTag;
    list.appendChild(
      el(
        "div",
        "empty",
        blank
          ? "No snippets yet. Create one with New snippet, or press your launcher shortcut on any page."
          : "No snippets.",
      ),
    );
    visibleItems = [];
    return;
  }
  // Cap rendered rows so a multi-thousand library stays snappy; the cap
  // only bites when nothing is filtering it down.
  const items = all.length > MAX_LIST_ROWS ? all.slice(0, MAX_LIST_ROWS) : all;
  visibleItems = items;
  items.forEach((s, index) => {
    const row = el("div", "row");
    row.dataset.id = s.id;
    const active = selectedIds.size ? selectedIds.has(s.id) : s.id === selectedId;
    row.classList.toggle("active", active);
    if (s.source === "personal") {
      row.draggable = true;
      row.addEventListener("dragstart", (e) => {
        e.dataTransfer.setData("text/snippet-id", s.id);
        e.dataTransfer.effectAllowed = "move";
        row.classList.add("dragging");
      });
      row.addEventListener("dragend", () => row.classList.remove("dragging"));
    }

    const t = el("div", "t");
    const titleText = el("span", "t-text");
    appendHighlighted(titleText, s.title || "(untitled)", q);
    t.appendChild(titleText);
    const right = el("span", "t-right");
    if (settings.show_usage_count !== false && s.uses) {
      right.appendChild(el("span", "uses", String(s.uses)));
    }
    if (s.source === "library") {
      const cloud = el("span", "cloud", "☁");
      cloud.title = "Shared team snippet";
      right.appendChild(cloud);
    }
    if (right.childNodes.length) t.appendChild(right);
    row.appendChild(t);

    if (s.body) {
      const body = el("div", "body");
      renderBodySample(body, s.body, q);
      row.appendChild(body);
    }

    const tags = s.tags || [];
    if (s.folder_path || tags.length) {
      const meta = el("div", "row-meta");
      if (s.folder_path) meta.appendChild(el("span", "folder", `${ICON_FOLDER} ${s.folder_path}`));
      if (tags.length) {
        const tagWrap = el("span", "tags");
        for (const tag of tags) tagWrap.appendChild(el("span", "tag", tag));
        meta.appendChild(tagWrap);
      }
      row.appendChild(meta);
    }

    row.addEventListener("click", (e) => handleRowClick(s, index, e));
    list.appendChild(row);
  });
  if (all.length > items.length) {
    list.appendChild(el("div", "empty", `Showing first ${items.length} of ${all.length}. Refine your search.`));
  }
}

// Explorer-style selection: plain click opens the snippet; Ctrl/Cmd
// toggles; Shift extends a range from the anchor. More than one
// selected swaps the editor for the bulk-action panel.
function handleRowClick(s, index, e) {
  if (e.shiftKey && anchorIndex != null) {
    const lo = Math.min(anchorIndex, index);
    const hi = Math.max(anchorIndex, index);
    selectedIds = new Set();
    for (let k = lo; k <= hi; k++) if (visibleItems[k]) selectedIds.add(visibleItems[k].id);
  } else if (e.ctrlKey || e.metaKey) {
    if (selectedIds.has(s.id)) selectedIds.delete(s.id);
    else selectedIds.add(s.id);
    anchorIndex = index;
  } else {
    navPane = "list";
    applyNavHighlight();
    anchorIndex = index;
    openEditor(s);
    return;
  }
  if (selectedIds.size > 1) {
    selectedId = null;
    renderList();
    renderBulkPanel();
  } else if (selectedIds.size === 1) {
    const only = snippets.find((x) => x.id === [...selectedIds][0]);
    if (only) openEditor(only);
  } else {
    renderList();
    clearEditor();
  }
}

function clearEditor() {
  selectedId = null;
  $("editor").replaceChildren(el("p", "placeholder", "Select a snippet, or create a new one."));
}

function optionEl(value, label) {
  const o = document.createElement("option");
  o.value = value;
  o.textContent = label;
  return o;
}

// All folder paths (including ancestors and empty folders) for the
// bulk move dropdown.
function allFolderPaths() {
  const set = new Set();
  for (const s of snippets) {
    if (!s.folder_path) continue;
    const parts = s.folder_path.split("/");
    for (let i = 1; i <= parts.length; i++) set.add(parts.slice(0, i).join("/"));
  }
  for (const f of pendingFolders) set.add(f);
  return [...set].sort((a, b) => a.localeCompare(b));
}

function renderBulkPanel() {
  const editor = $("editor");
  editor.replaceChildren();
  const ids = [...selectedIds];
  const personal = ids.filter((id) =>
    snippets.some((s) => s.id === id && s.source === "personal"),
  );
  const libCount = ids.length - personal.length;

  editor.appendChild(el("h2", "bulk-title", `${ids.length} selected`));
  if (libCount) {
    editor.appendChild(el("div", "ro-note", `${libCount} team snippet${libCount > 1 ? "s" : ""} will be skipped (read-only).`));
  }

  const moveLabel = el("label", null, "Move selected to");
  const sel = el("select");
  sel.appendChild(optionEl("", "Unfiled"));
  for (const f of allFolderPaths()) sel.appendChild(optionEl(f, f));
  moveLabel.appendChild(sel);
  editor.appendChild(moveLabel);

  const actions = el("div", "actions");
  const moveBtn = el("button", "primary", "Move");
  moveBtn.addEventListener("click", () => bulkMove(personal, sel.value || null));
  const delBtn = el("button", "danger", "Delete selected");
  delBtn.addEventListener("click", () => bulkDelete(personal));
  const clearBtn = el("button", null, "Clear");
  clearBtn.addEventListener("click", () => {
    selectedIds = new Set();
    renderList();
    clearEditor();
  });
  actions.append(moveBtn, delBtn, clearBtn);
  editor.appendChild(actions);

  if (!personal.length) {
    moveBtn.disabled = true;
    delBtn.disabled = true;
  }
}

async function bulkMove(ids, folderPath) {
  for (const id of ids) {
    const s = snippets.find((x) => x.id === id);
    if (!s || s.source !== "personal") continue;
    if ((s.folder_path || null) === (folderPath || null)) continue;
    const payload = { title: s.title, body: s.body, tags: s.tags || [], folder_path: folderPath || null };
    const res = await send(MSG.SNIPPET_UPDATE, { id, expectedVersion: s.version, payload });
    if (!res.ok) {
      toast(res.error || SERVER_ERR, true);
      break;
    }
  }
  selectedIds = new Set();
  if (folderPath) {
    expandAncestors(folderPath);
    selectedFolder = folderPath;
  }
  await loadSnippets();
  clearEditor();
}

async function bulkDelete(ids) {
  if (!ids.length) return;
  if (!confirm(`Delete ${ids.length} snippet${ids.length > 1 ? "s" : ""}? They can be restored from Trash.`)) return;
  for (const id of ids) {
    const res = await send(MSG.SNIPPET_DELETE, { id });
    if (!res.ok) {
      toast(res.error || SERVER_ERR, true);
      break;
    }
  }
  selectedIds = new Set();
  await loadSnippets();
  clearEditor();
}

// ---- editor ----
function formatRules() {
  return settings.format_rules?.length ? settings.format_rules : DEFAULT_FORMAT_RULES;
}

function openEditor(snippet, prefill) {
  selectedId = snippet ? snippet.id : null;
  selectedIds = snippet ? new Set([snippet.id]) : new Set();
  renderList();
  const editor = $("editor");
  editor.replaceChildren();
  const readOnly = snippet?.source === "library";

  if (readOnly) {
    editor.appendChild(el("div", "ro-note", "Team library snippet (read-only; managed in the dashboard)."));
  }

  const fieldRow = el("div", "editor-fields");
  const title = inputField(fieldRow, "Title", snippet?.title || "", readOnly, "f-title");
  const folder = inputField(fieldRow, "Folder", snippet?.folder_path || "", readOnly, "f-folder");
  const tags = inputField(fieldRow, "Tags", (snippet?.tags || []).join(", "), readOnly, "f-tags");
  editor.appendChild(fieldRow);
  if (!snippet) title.focus(); // new snippet (incl. context-menu capture): jump to the title

  const bodyLabel = el("label", "f-body", "Body");
  const body = el("textarea");
  body.value = snippet?.body || prefill?.body || "";
  body.disabled = readOnly;
  bodyLabel.appendChild(body);
  editor.appendChild(bodyLabel);

  // Live preview with variables highlighted.
  const previewWrap = el("div", "editor-preview-wrap");
  previewWrap.appendChild(el("div", "preview-label", "Preview"));
  const preview = el("div", "editor-preview");
  previewWrap.appendChild(preview);
  editor.appendChild(previewWrap);
  const renderPrev = () => renderFormatted(preview, body.value, formatRules());
  renderPrev();
  body.addEventListener("input", renderPrev);

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
    await afterUpsert(snippet ? snippet.id : res.data?.id);
  });
}

// Reload, jump to the snippet's folder, open it, and flash its row so
// a freshly created/duplicated snippet is never lost off-screen.
async function afterUpsert(id) {
  await loadSnippets();
  const fresh = snippets.find((s) => s.id === id);
  if (!fresh) return;
  selectedFolder = fresh.folder_path || UNFILED;
  if (fresh.folder_path) expandAncestors(fresh.folder_path);
  renderTree();
  openEditor(fresh);
  const row = $("list").querySelector(`[data-id="${id}"]`);
  if (row) {
    row.scrollIntoView({ block: "nearest" });
    row.classList.add("flash");
    setTimeout(() => row.classList.remove("flash"), 1200);
  }
}

function inputField(parent, labelText, value, disabled, cls) {
  const label = el("label", cls || null, labelText);
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
  if (res.ok) await afterUpsert(res.data?.id);
}

async function remove(snippet) {
  if (!confirm(`Delete "${snippet.title}"? It can be restored from Trash.`)) return;
  const res = await send(MSG.SNIPPET_DELETE, { id: snippet.id });
  if (res.ok) {
    selectedId = null;
    await loadSnippets();
    $("editor").replaceChildren(el("p", "placeholder", "Select a snippet, or create a new one."));
  } else {
    toast(res.error || SERVER_ERR, true);
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
  $("set-theme").value = settings.theme || "dark";
  $("set-compact").checked = !!settings.compact;
  editingRules = (settings.format_rules?.length ? settings.format_rules : DEFAULT_FORMAT_RULES).map((r) => ({ ...r }));
  renderRuleEditor();
}

// Editable table of format rules (label / prefix / suffix), mirroring the
// desktop. The preview reads these to decide what and how to format.
function renderRuleEditor() {
  const rows = $("rule-rows");
  rows.replaceChildren();
  editingRules.forEach((rule, i) => {
    const tr = el("tr");
    for (const field of ["label", "prefix", "suffix"]) {
      const td = el("td");
      const input = el("input");
      input.type = "text";
      input.value = rule[field] || "";
      input.addEventListener("input", () => {
        rule[field] = input.value;
      });
      td.appendChild(input);
      tr.appendChild(td);
    }
    const delTd = el("td");
    const del = el("button", "rule-del", "✕");
    del.type = "button";
    del.title = "Remove rule";
    del.addEventListener("click", () => {
      editingRules.splice(i, 1);
      renderRuleEditor();
    });
    delTd.appendChild(del);
    tr.appendChild(delTd);
    rows.appendChild(tr);
  });
}

function applyTheme() {
  document.documentElement.dataset.theme = settings.theme === "light" ? "light" : "dark";
}

function applyDensity() {
  document.body.classList.toggle("compact", !!settings.compact);
}

async function saveSettings() {
  const patch = {
    show_savings_estimate: $("set-show-savings").checked,
    typing_speed_wpm: Number($("set-wpm").value) || 40,
    hourly_wage: Number($("set-wage").value) || 0,
    wage_currency: $("set-currency").value || "$",
    sort_by_usage: $("set-sort-usage").checked,
    show_usage_count: $("set-usage-count").checked,
    theme: $("set-theme").value,
    compact: $("set-compact").checked,
    format_rules: editingRules
      .map((r) => ({ label: (r.label || "").trim(), prefix: r.prefix || "", suffix: r.suffix || "" }))
      .filter((r) => r.label || r.prefix || r.suffix),
  };
  const res = await send(MSG.SETTINGS_SET, { patch });
  const status = $("settings-status");
  if (res.ok) {
    settings = res.data;
    applyTheme();
    applyDensity();
    status.textContent = "Saved.";
    status.className = "status ok";
    setTimeout(() => {
      if (status.textContent === "Saved.") {
        status.textContent = "";
        status.className = "status";
      }
    }, 2500);
    renderList();
  } else {
    status.textContent = res.error || "Save failed.";
    status.className = "status err";
  }
}

// ---- trash ----
let trashItems = [];

async function loadTrash() {
  // Render local tombstones immediately (instant offline), then fold in
  // the server's trash if it's reachable.
  const local = await send(MSG.TRASH_LIST, { localOnly: true });
  if (local.ok) {
    trashItems = local.data || [];
    renderTrash();
  }
  const merged = await send(MSG.TRASH_LIST);
  if (merged.ok) {
    trashItems = merged.data || [];
    renderTrash();
  } else if (!local.ok) {
    $("trash-list").replaceChildren(el("div", "muted", merged.error || "Could not load trash."));
  }
}

function trashMatches(item, q) {
  if (!q) return true;
  const p = item.payload || {};
  const hay = [p.title, p.body, p.folder_path, ...(p.tags || [])].join(" ").toLowerCase();
  return hay.includes(q);
}

function renderTrash() {
  const list = $("trash-list");
  const q = ($("trash-search").value || "").trim().toLowerCase();
  list.replaceChildren();
  if (trashItems.length === 0) {
    list.appendChild(el("div", "muted", "Trash is empty."));
    return;
  }
  const items = trashItems.filter((it) => trashMatches(it, q));
  if (items.length === 0) {
    list.appendChild(el("div", "muted", "No matches."));
    return;
  }
  for (const item of items) {
    const p = item.payload || {};
    const card = el("div", "trash-item");

    const head = el("div", "trash-head");
    const titleEl = el("div", "t-text");
    appendHighlighted(titleEl, p.title || "(untitled)", q);
    head.appendChild(titleEl);
    const restore = el("button", "primary", "Restore");
    restore.addEventListener("click", async () => {
      restore.disabled = true;
      const r = await send(MSG.TRASH_RESTORE, { id: item.id });
      if (r.ok) {
        await loadSnippets();
        loadTrash();
      } else {
        restore.disabled = false;
        toast(r.error || SERVER_ERR, true);
      }
    });
    head.appendChild(restore);
    card.appendChild(head);

    if (p.body) {
      const body = el("div", "body");
      renderBodySample(body, p.body, q);
      card.appendChild(body);
    }

    const tags = p.tags || [];
    if (p.folder_path || tags.length || item.deleted_at) {
      const meta = el("div", "row-meta");
      if (p.folder_path) meta.appendChild(el("span", "folder", `${ICON_FOLDER} ${p.folder_path}`));
      if (tags.length) {
        const tagWrap = el("span", "tags");
        for (const tag of tags) tagWrap.appendChild(el("span", "tag", tag));
        meta.appendChild(tagWrap);
      }
      if (item.deleted_at) {
        meta.appendChild(el("span", "trash-when", `deleted ${formatWhen(item.deleted_at)}`));
      }
      card.appendChild(meta);
    }

    list.appendChild(card);
  }
}

// Unix seconds to a short relative label.
function formatWhen(unixSeconds) {
  const diff = Date.now() / 1000 - unixSeconds;
  if (diff < 60) return "just now";
  if (diff < 3600) return `${Math.round(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.round(diff / 3600)}h ago`;
  return `${Math.round(diff / 86400)}d ago`;
}

// ---- import / export ----
// A folder-grouped tree of snippets with per-item and per-folder
// checkboxes (mirrors the desktop/dashboard flow). Calls onConfirm with
// the chosen items.
function openSelectionModal({ heading, items, confirmLabel, onConfirm }) {
  const back = el("div", "modal-back");
  const box = el("div", "modal modal-wide");
  box.appendChild(el("h2", null, heading));

  const selected = new Set(items.map((_, i) => i));

  const masterRow = el("label", "sel-master");
  const master = document.createElement("input");
  master.type = "checkbox";
  masterRow.appendChild(master);
  masterRow.appendChild(el("span", null, "Select all"));
  box.appendChild(masterRow);

  const listEl = el("div", "sel-list");
  const groups = new Map();
  items.forEach((it, i) => {
    const f = it.folder_path || "";
    if (!groups.has(f)) groups.set(f, []);
    groups.get(f).push(i);
  });

  const itemChecks = [];
  const folderChecks = [];
  for (const f of [...groups.keys()].sort((a, b) => a.localeCompare(b))) {
    const idxs = groups.get(f);
    const fRow = el("label", "sel-folder");
    const fc = document.createElement("input");
    fc.type = "checkbox";
    fRow.appendChild(fc);
    fRow.appendChild(el("span", "sel-folder-label", f ? `${ICON_FOLDER} ${f}` : `${ICON_UNFILED} Unfiled`));
    listEl.appendChild(fRow);
    fc.addEventListener("change", () => {
      for (const i of idxs) (fc.checked ? selected.add(i) : selected.delete(i));
      syncChecks();
    });
    folderChecks.push({ input: fc, idxs });
    for (const i of idxs) {
      const row = el("label", "sel-item");
      const cb = document.createElement("input");
      cb.type = "checkbox";
      row.appendChild(cb);
      row.appendChild(el("span", "t-text", items[i].title || "(untitled)"));
      listEl.appendChild(row);
      cb.addEventListener("change", () => {
        cb.checked ? selected.add(i) : selected.delete(i);
        syncChecks();
      });
      itemChecks.push({ input: cb, i });
    }
  }
  box.appendChild(listEl);

  const actions = el("div", "modal-actions");
  const go = el("button", "primary", confirmLabel);
  go.addEventListener("click", () => {
    back.remove();
    onConfirm(items.filter((_, i) => selected.has(i)));
  });
  const cancel = el("button", null, "Cancel");
  cancel.addEventListener("click", () => back.remove());
  actions.append(go, cancel);
  box.appendChild(actions);

  function syncChecks() {
    for (const { input, i } of itemChecks) input.checked = selected.has(i);
    for (const { input, idxs } of folderChecks) {
      const on = idxs.filter((i) => selected.has(i)).length;
      input.checked = on === idxs.length;
      input.indeterminate = on > 0 && on < idxs.length;
    }
    master.checked = selected.size === items.length;
    master.indeterminate = selected.size > 0 && selected.size < items.length;
    go.disabled = selected.size === 0;
    go.textContent = `${confirmLabel} (${selected.size})`;
  }
  master.addEventListener("change", () => {
    selected.clear();
    if (master.checked) items.forEach((_, i) => selected.add(i));
    syncChecks();
  });

  back.appendChild(box);
  back.addEventListener("mousedown", (e) => {
    if (e.target === back) back.remove();
  });
  document.body.appendChild(back);
  syncChecks();
}

function exportSnippets() {
  const personal = snippets
    .filter((s) => s.source === "personal")
    .map((s) => ({ title: s.title, body: s.body, tags: s.tags || [], folder_path: s.folder_path || null }));
  if (!personal.length) {
    toast("No personal snippets to export.");
    return;
  }
  openSelectionModal({
    heading: "Export snippets",
    items: personal,
    confirmLabel: "Export",
    onConfirm: (chosen) => {
      const blob = new Blob([JSON.stringify({ version: 1, snippets: chosen }, null, 2)], {
        type: "application/json",
      });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = "snipdesk-snippets.json";
      a.click();
      URL.revokeObjectURL(url);
    },
  });
}

async function importFromFile(file) {
  if (!file) return;
  let parsed;
  try {
    parsed = JSON.parse(await file.text());
  } catch {
    openModal("Import failed", "That file is not valid JSON.", [{ label: "OK" }]);
    return;
  }
  const incoming = Array.isArray(parsed) ? parsed : parsed?.snippets;
  if (!Array.isArray(incoming)) {
    openModal("Import failed", "No snippets found in that file.", [{ label: "OK" }]);
    return;
  }
  const valid = [];
  for (const raw of incoming) {
    const payload = {
      title: String(raw?.title ?? "").trim(),
      body: String(raw?.body ?? ""),
      tags: Array.isArray(raw?.tags) ? raw.tags.map(String) : [],
      folder_path: raw?.folder_path ? String(raw.folder_path) : null,
    };
    if (!validateSnippet(payload)) valid.push(payload);
  }
  if (!valid.length) {
    openModal("Nothing to import", "No valid snippets were found in that file.", [{ label: "OK" }]);
    return;
  }
  openSelectionModal({
    heading: "Import snippets",
    items: valid,
    confirmLabel: "Import",
    onConfirm: async (chosen) => {
      for (const payload of chosen) {
        const res = await send(MSG.SNIPPET_CREATE, { payload });
        if (!res.ok) {
          toast(res.error || SERVER_ERR, true);
          break;
        }
      }
      await loadSnippets();
      toast(`Imported ${chosen.length} snippet${chosen.length > 1 ? "s" : ""}.`);
    },
  });
}

// Arrow keys move through the visible list; Enter opens the editor.
function moveListSel(dir) {
  if (!visibleItems.length) return;
  let idx = visibleItems.findIndex((s) => s.id === selectedId);
  idx = dir === "down" ? (idx < 0 ? 0 : Math.min(visibleItems.length - 1, idx + 1)) : idx <= 0 ? 0 : idx - 1;
  const s = visibleItems[idx] || visibleItems[0];
  openEditor(s);
  $("list").querySelector(`[data-id="${s.id}"]`)?.scrollIntoView({ block: "nearest" });
}

// Arrow keys drive navigation globally (they have no other use here): Up/Down
// move the active section's selection, Left/Right switch which section is
// active, Enter expands/collapses the active folder. Real text fields keep
// their own arrow behavior; the search box is the one field arrows escape.
function onGlobalNavKey(e) {
  if (!e.key.startsWith("Arrow") && e.key !== "Enter") return;
  if (e.ctrlKey || e.metaKey || e.altKey) return;
  if (document.querySelector(".modal-back:not(.hidden)")) return;
  const a = document.activeElement;
  if (a && a !== $("search") && (a.tagName === "INPUT" || a.tagName === "TEXTAREA" || a.isContentEditable)) return;
  if (e.key === "ArrowLeft") {
    e.preventDefault();
    navPane = "tree";
    applyNavHighlight();
  } else if (e.key === "ArrowRight") {
    e.preventDefault();
    if (!visibleItems.length) return; // nothing to jump to; stay on folders
    navPane = "list";
    applyNavHighlight();
    // Land on the top snippet so no extra keypress is needed to highlight one.
    if (!visibleItems.some((s) => s.id === selectedId)) openEditor(visibleItems[0]);
  } else if (e.key === "ArrowUp") {
    e.preventDefault();
    navPane === "tree" ? moveTreeSel("up") : moveListSel("up");
  } else if (e.key === "ArrowDown") {
    e.preventDefault();
    navPane === "tree" ? moveTreeSel("down") : moveListSel("down");
  } else if (e.key === "Enter" && navPane === "tree") {
    e.preventDefault();
    toggleFolderExpand(selectedFolder);
  }
}

// ---- wiring ----
function wire() {
  $("btn-trash").addEventListener("click", () => openModalEl("trash-modal"));
  $("btn-settings").addEventListener("click", () => openModalEl("settings-modal"));
  for (const btn of document.querySelectorAll(".modal-close")) {
    btn.addEventListener("click", () => closeModalEl(btn.dataset.close));
  }
  for (const back of document.querySelectorAll("#settings-modal, #trash-modal")) {
    back.addEventListener("mousedown", (e) => {
      if (e.target === back) closeModalEl(back.id);
    });
  }
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeAllModals();
  });
  // Type-to-search: a printable key with no modifier, when focus isn't in a
  // field and no modal is open, jumps to the search box so the character
  // lands there.
  document.addEventListener("keydown", (e) => {
    if (e.key.length !== 1 || e.ctrlKey || e.metaKey || e.altKey) return;
    const a = document.activeElement;
    const editable =
      a && (a.tagName === "INPUT" || a.tagName === "TEXTAREA" || a.tagName === "SELECT" || a.isContentEditable);
    if (editable) return;
    if (document.querySelector(".modal-back:not(.hidden)")) return;
    navPane = "list"; // typing means searching, so arrows then walk results
    applyNavHighlight();
    $("search").focus();
  });
  let searchTimer;
  $("search").addEventListener("input", () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(renderList, 90);
  });
  $("folder-create").addEventListener("submit", (e) => {
    e.preventDefault();
    createFolder();
  });
  $("btn-new").addEventListener("click", () => openEditor(null));
  $("btn-export").addEventListener("click", exportSnippets);
  $("btn-import").addEventListener("click", () => $("import-file").click());
  $("import-file").addEventListener("change", (e) => {
    const file = e.target.files?.[0];
    e.target.value = ""; // allow re-importing the same file
    importFromFile(file);
  });
  document.addEventListener("keydown", onGlobalNavKey);
  $("btn-dashboard").addEventListener("click", () => {
    const base = (settings.server_url || "").replace(/\/+$/, "");
    if (base) chrome.tabs.create({ url: `${base}/dashboard/library` });
  });
  $("btn-shortcut").addEventListener("click", () =>
    chrome.tabs.create({ url: "chrome://extensions/shortcuts" }),
  );
  $("ob-next").addEventListener("click", async () => {
    if (obStep >= OB_STEPS.length - 1) { finishOnboarding(); return; }
    await commitOnboardingStep(OB_STEPS[obStep]);
    obStep += 1;
    renderOnboarding();
  });
  $("ob-back").addEventListener("click", () => {
    if (obStep > 0) { obStep -= 1; renderOnboarding(); }
  });
  $("ob-skip").addEventListener("click", finishOnboarding);
  $("ob-typing-input").addEventListener("input", onTypingInput);
  $("ob-typing-restart").addEventListener("click", primeTyping);
  $("ob-shortcut-link").addEventListener("click", (e) => {
    e.preventDefault();
    chrome.tabs.create({ url: "chrome://extensions/shortcuts" });
  });
  $("btn-replay-onboarding").addEventListener("click", () => {
    closeModalEl("settings-modal");
    showOnboarding();
  });
  $("btn-add-rule").addEventListener("click", () => {
    editingRules.push({ label: "New", prefix: "", suffix: "" });
    renderRuleEditor();
  });
  $("btn-reset-rules").addEventListener("click", () => {
    editingRules = DEFAULT_FORMAT_RULES.map((r) => ({ ...r }));
    renderRuleEditor();
  });
  $("trash-search").addEventListener("input", renderTrash);
  $("btn-save-settings").addEventListener("click", saveSettings);
  $("btn-logout").addEventListener("click", async () => {
    await send(MSG.AUTH_LOGOUT);
    location.reload();
  });
}

// Catch a context-menu capture when the manager tab is already open.
chrome.storage.onChanged.addListener((changes, area) => {
  const text = area === "local" && changes.pending_new_snippet?.newValue;
  if (text) {
    clearPendingNewSnippet();
    closeAllModals();
    openEditor(null, { body: text });
  }
});

init();
