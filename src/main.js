import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import { check as checkUpdate } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

// Vite substitutes the literal - esbuild dead-code-eliminates `if (false)` branches. See vite.config.js.
const TEAMS_BUILD = __SNIPDESK_TEAMS_BUILD__;

// ---------- Helpers ----------
function asPath(v) {
  // openDialog may return string, { path }, or null depending on options/version.
  if (!v) return null;
  if (typeof v === "string") return v;
  if (Array.isArray(v)) return v[0] || null;
  if (v.path) return v.path;
  return String(v);
}

// Inline ghost-text autosuggest (fish/zsh-style trailing completion).
//
// Keymap:
//   Tab         - commit ghost; subsequent Tabs cycle candidates. Falls through to focus-move when no candidates.
//   Right Arrow - commit and exit cycle mode.
//   Escape / arrows / Backspace / mid-line click - dismiss without commit.
//
// Cycle order is whatever getOptions() returns; callers own ranking.
// Name is attachCombobox() for legacy call sites - no dropdown anymore.
function attachCombobox(input, getOptions) {
  // Idempotent (safe across hot-reload).
  if (input.dataset.ghostAutosuggest === "1") return;
  input.dataset.ghostAutosuggest = "1";

  const wrap = document.createElement("div");
  wrap.className = "ghost-input";
  input.parentNode.insertBefore(wrap, input);
  wrap.appendChild(input);
  input.classList.add("ghost-input-real");
  input.setAttribute("autocomplete", "off");
  // Strip native datalist so the OS dropdown doesn't double up on the ghost.
  input.removeAttribute("list");

  // Shadow div is absolutely positioned over the wrapper. Typed span is transparent
  // (pushes the suffix horizontally without painting); suffix span renders the ghost.
  const shadow = document.createElement("div");
  shadow.className = "ghost-input-shadow";
  shadow.setAttribute("aria-hidden", "true");
  const typedSpan = document.createElement("span");
  typedSpan.className = "ghost-input-typed";
  const suffixSpan = document.createElement("span");
  suffixSpan.className = "ghost-input-suffix";
  shadow.appendChild(typedSpan);
  shadow.appendChild(suffixSpan);
  wrap.appendChild(shadow);

  // Copy padding/font/border from the input's computed style so the ghost aligns
  // pixel-for-pixel. Padding varies by context (modal-card 8px, var-field 6px),
  // so static CSS won't work. Resync on first focus because var-prompt attaches
  // while the modal is still display:none and computed values are useless then.
  let synced = false;
  function syncShadowStyles() {
    const cs = window.getComputedStyle(input);
    if (!cs || !cs.fontSize) return;
    shadow.style.fontSize = cs.fontSize;
    shadow.style.fontFamily = cs.fontFamily;
    shadow.style.fontWeight = cs.fontWeight;
    shadow.style.fontStyle = cs.fontStyle;
    shadow.style.lineHeight = cs.lineHeight;
    shadow.style.letterSpacing = cs.letterSpacing;
    shadow.style.padding = cs.padding;
    shadow.style.borderWidth = cs.borderWidth;
    shadow.style.borderStyle = cs.borderStyle;
    // Real input draws the visible frame; shadow only borrows metrics.
    shadow.style.borderColor = "transparent";
    shadow.style.borderRadius = cs.borderRadius;
    synced = true;
  }
  syncShadowStyles();

  // candidates: prefix-matching options (case-insensitive, excludes exact match).
  // cycleIndex: ghost index, -1 = nothing armed.
  // mutating: reentrancy guard around commitCandidate's programmatic value write.
  let candidates = [];
  let cycleIndex = -1;
  let mutating = false;

  function caretAtEnd() {
    return (
      input.selectionStart === input.value.length &&
      input.selectionEnd === input.value.length
    );
  }

  function clearGhost() {
    typedSpan.textContent = "";
    suffixSpan.textContent = "";
  }

  function paintGhost() {
    if (cycleIndex < 0 || cycleIndex >= candidates.length) {
      clearGhost();
      return;
    }
    const candidate = candidates[cycleIndex];
    const value = input.value;
    if (
      candidate.length <= value.length ||
      !candidate.toLowerCase().startsWith(value.toLowerCase())
    ) {
      // Stale candidate - prefix invariant broken.
      clearGhost();
      return;
    }
    typedSpan.textContent = value;
    suffixSpan.textContent = candidate.slice(value.length);
  }

  function resetCycle() {
    candidates = [];
    cycleIndex = -1;
    clearGhost();
  }

  function recompute() {
    const value = input.value;
    if (!value || !caretAtEnd()) {
      resetCycle();
      return;
    }
    const all = getOptions() || [];
    const lower = value.toLowerCase();
    const matches = all.filter(
      (o) =>
        typeof o === "string" &&
        o.toLowerCase() !== lower &&
        o.toLowerCase().startsWith(lower)
    );
    if (matches.length === 0) {
      resetCycle();
      return;
    }
    candidates = matches;
    cycleIndex = 0;
    paintGhost();
  }

  // advanceCycle=true keeps cycle state for the next Tab; false exits cycle mode.
  function commitCandidate(candidate, advanceCycle) {
    mutating = true;
    input.value = candidate;
    const len = candidate.length;
    input.selectionStart = len;
    input.selectionEnd = len;
    if (advanceCycle && candidates.length > 0) {
      cycleIndex = (cycleIndex + 1) % candidates.length;
    } else {
      candidates = [];
      cycleIndex = -1;
    }
    clearGhost();
    // Programmatic .value writes don't fire input natively.
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
    mutating = false;
  }

  // ---- Listeners ----

  input.addEventListener("input", (ev) => {
    if (mutating) return;
    // Backspace / cut / line-delete dismiss without recomputing.
    if (ev.inputType && ev.inputType.startsWith("delete")) {
      resetCycle();
      return;
    }
    recompute();
  });

  input.addEventListener("focus", () => {
    // Retry sync if the first call ran while the modal was display:none.
    if (!synced) syncShadowStyles();
    if (input.value && caretAtEnd()) recompute();
  });

  input.addEventListener("blur", () => {
    resetCycle();
  });

  input.addEventListener("click", () => {
    // Caret in the middle = user editing inline; drop the ghost.
    if (!caretAtEnd()) resetCycle();
  });

  input.addEventListener("keydown", (ev) => {
    // Tab: commit + cycle. Falls through to default focus-move when no candidate.
    if (ev.key === "Tab" && !ev.shiftKey) {
      if (candidates.length > 0 && cycleIndex >= 0) {
        ev.preventDefault();
        commitCandidate(candidates[cycleIndex], /* advanceCycle */ true);
      }
      return;
    }

    // Right Arrow at end with ghost armed: commit + exit cycle.
    if (
      ev.key === "ArrowRight" &&
      caretAtEnd() &&
      candidates.length > 0 &&
      cycleIndex >= 0
    ) {
      ev.preventDefault();
      commitCandidate(candidates[cycleIndex], /* advanceCycle */ false);
      return;
    }

    // Escape: dismiss. stopPropagation so the modal's Escape-to-close doesn't
    // also fire on the same keystroke.
    if (ev.key === "Escape" && cycleIndex >= 0) {
      ev.stopPropagation();
      ev.preventDefault();
      resetCycle();
      return;
    }

    // Caret-moving keys: defer to next tick so the browser has already moved.
    if (
      ev.key === "ArrowLeft" ||
      ev.key === "ArrowUp" ||
      ev.key === "ArrowDown" ||
      ev.key === "Home" ||
      ev.key === "PageUp" ||
      ev.key === "PageDown"
    ) {
      setTimeout(() => {
        if (!caretAtEnd()) resetCycle();
      }, 0);
    }
  });
}

// null = "All snippets"; "__root__" = unfiled; "__team__" = team-library cache.
const ALL_FOLDERS = null;
const ROOT_FOLDER = "__root__";
const TEAM_FOLDER = "__team__";

// Must match settings.rs default_format_rules().
const DEFAULT_FORMAT_RULES = [
  { label: "Bold", prefix: "**", suffix: "**" },
  { label: "Italic", prefix: "*", suffix: "*" },
  { label: "Code", prefix: "`", suffix: "`" },
  { label: "Link", prefix: "[", suffix: "](https://)" },
];

// ---------- State ----------
const state = {
  snippets: [],
  // Unfiltered list - used only by the savings estimator so its readout doesn't
  // change with the search box.
  allSnippets: [],
  tags: [],
  folders: [], // [{ path, has_snippets }]
  expandedFolders: new Set(),
  selectedFolder: ALL_FOLDERS,
  selectedIndex: 0,
  // Multi-select set (snippet IDs). Always contains selectedIndex's row; single-
  // select is just size==1. Plain click resets, Ctrl toggles, Shift extends.
  selectedIds: new Set(),
  // Shift-click anchor. null = fall back to selectedIndex.
  anchorIndex: null,
  // Folder sidebar multi-selection. Real folders only - pseudo-nodes never
  // participate. Independent of `selectedFolder` (which still drives the preview).
  selectedFolderPaths: new Set(),
  folderAnchor: null,
  activeTag: null,
  settings: null,
  editingId: null, // null = creating
  pendingPaste: null, // { snippet, copyOnly }
  // Format-rule working copy; flushed to settings.format_rules on Save.
  editingRules: [],
  pendingDuplicateSave: null,
  // Mirrors backend SyncStatus.
  teamStatus: { fetched_at_unix: null, snippet_count: 0, last_error: null },
  // Build-time defaults baked in by scripts/brand.mjs. Populated once
  // in init() on Teams builds. When `server_url` is non-empty, the
  // build is whitelabel-locked and the UI hides the URL inputs.
  brandDefaults: { server_url: "", sso_only: false },
  // Cached /api/auth/methods response. The Team Library sign-in
  // surface (password fields, provider buttons, paste-fallback) is
  // shown/hidden based on this. null = not fetched yet; render
  // falls back to a conservative "URL input only" until the first
  // successful fetch.
  serverMethods: null,
  // { start, end, scrollTop } captured when Link button was clicked.
  pendingLinkInsert: null,
};

// ---------- Element refs ----------
const els = {
  search: document.getElementById("search"),
  list: document.getElementById("snippet-list"),
  preview: document.getElementById("preview"),
  tagStrip: document.getElementById("tag-strip"),
  status: document.getElementById("status"),
  btnNew: document.getElementById("btn-new"),
  btnSettings: document.getElementById("btn-settings"),

  pane: document.getElementById("pane"),
  folderSidebar: document.getElementById("folder-sidebar"),
  folderTree: document.getElementById("folder-tree"),
  btnNewFolder: document.getElementById("btn-new-folder"),

  editor: document.getElementById("editor"),
  editorTitle: document.getElementById("editor-title"),
  editorTitleInput: document.getElementById("editor-title-input"),
  editorFolderInput: document.getElementById("editor-folder-input"),
  editorTagsInput: document.getElementById("editor-tags-input"),
  editorBodyInput: document.getElementById("editor-body-input"),
  editorError: document.getElementById("editor-error"),
  editorFormatToolbar: document.getElementById("editor-format-toolbar"),
  editorSave: document.getElementById("editor-save"),
  editorCancel: document.getElementById("editor-cancel"),

  // Duplicate-title warning modal
  dupWarn: document.getElementById("dup-warn"),
  dupWarnMsg: document.getElementById("dup-warn-msg"),
  dupOpenExisting: document.getElementById("dup-open-existing"),
  dupEditTitle: document.getElementById("dup-edit-title"),
  dupSaveAnyway: document.getElementById("dup-save-anyway"),

  varPrompt: document.getElementById("var-prompt"),
  varFields: document.getElementById("var-fields"),
  varSubmit: document.getElementById("var-submit"),
  varCancel: document.getElementById("var-cancel"),

  // Link insert modal (Link button in editor toolbar)
  linkPrompt: document.getElementById("link-prompt"),
  linkTextInput: document.getElementById("link-text-input"),
  linkUrlInput: document.getElementById("link-url-input"),
  linkInsert: document.getElementById("link-insert"),
  linkCancel: document.getElementById("link-cancel"),

  settings: document.getElementById("settings"),
  // General tab
  setPasteMode: document.getElementById("set-paste-mode"),
  setDelay: document.getElementById("set-delay"),
  setSortMode: document.getElementById("set-sort-mode"),
  setClose: document.getElementById("set-close"),
  setAutostart: document.getElementById("set-autostart"),
  setCloseToTray: document.getElementById("set-close-to-tray"),
  setMinimizeToTray: document.getElementById("set-minimize-to-tray"),
  setStartInTray: document.getElementById("set-start-in-tray"),
  // Appearance tab
  setTheme: document.getElementById("set-theme"),
  setAccentColor: document.getElementById("set-accent-color"),
  setAccentText: document.getElementById("set-accent-text"),
  btnAccentReset: document.getElementById("btn-accent-reset"),
  accentPreview: document.getElementById("accent-preview"),
  setCompact: document.getElementById("set-compact"),
  setShowUsage: document.getElementById("set-show-usage"),
  setHideOnBlur: document.getElementById("set-hide-on-blur"),
  setAlwaysOnTop: document.getElementById("set-always-on-top"),
  // Hotkeys tab
  setHotkey: document.getElementById("set-hotkey"),
  setQuickAddHotkey: document.getElementById("set-quick-add-hotkey"),
  // Team Library tab. The legacy library-URL fields
  // (set-team-url / set-team-interval / set-team-folder-name /
  // set-team-startup) were removed from the UI as part of the v1
  // de-bloat; the underlying settings.team_library_url plumbing
  // still works for anyone who set it via settings.json before the
  // UI removal, but there's no surface to configure it from the
  // app any more.
  setShowTeamInline: document.getElementById("set-show-team-inline"),
  // Teams: server sync section
  serverSignedOut: document.getElementById("server-signed-out"),
  serverSignedIn: document.getElementById("server-signed-in"),
  setServerUrl: document.getElementById("set-server-url"),
  setServerEmail: document.getElementById("set-server-email"),
  setServerPassword: document.getElementById("set-server-password"),
  btnServerLogin: document.getElementById("btn-server-login"),
  btnServerSignup: document.getElementById("btn-server-signup"),
  btnServerLogout: document.getElementById("btn-server-logout"),
  btnServerSync: document.getElementById("btn-server-sync"),
  btnServerOidc: document.getElementById("btn-server-oidc"),
  serverProviders: document.getElementById("server-providers"),
  serverPasswordSection: document.getElementById("server-password-section"),
  serverAuthDivider: document.getElementById("server-auth-divider"),
  serverPasteFallback: document.getElementById("server-paste-fallback"),
  setServerPasteToken: document.getElementById("set-server-paste-token"),
  btnServerPasteToken: document.getElementById("btn-server-paste-token"),
  serverError: document.getElementById("server-error"),
  serverUserName: document.getElementById("server-user-name"),
  serverUserEmail: document.getElementById("server-user-email"),
  serverUserRole: document.getElementById("server-user-role"),
  serverUrlDisplay: document.getElementById("server-url-display"),
  serverLastSync: document.getElementById("server-last-sync"),
  serverSyncDetail: document.getElementById("server-sync-detail"),
  serverLastResult: document.getElementById("server-last-result"),
  // Trash modal (Teams only)
  trashModal: document.getElementById("trash-modal"),
  trashList: document.getElementById("trash-list"),
  trashClose: document.getElementById("trash-close"),
  // Editor tab
  ruleRows: document.getElementById("rule-rows"),
  btnAddRule: document.getElementById("btn-add-rule"),
  btnResetRules: document.getElementById("btn-reset-rules"),
  // Savings tab
  setShowSavings: document.getElementById("set-show-savings"),
  setWpm: document.getElementById("set-wpm"),
  setWage: document.getElementById("set-wage"),
  setCurrency: document.getElementById("set-currency"),
  savings: document.getElementById("savings"),
  // About tab
  setBackupDays: document.getElementById("set-backup-days"),
  setLogDays: document.getElementById("set-log-days"),
  btnOpenBackups: document.getElementById("btn-open-backups"),
  btnOpenLogs: document.getElementById("btn-open-logs"),
  logPathDisplay: document.getElementById("log-path-display"),
  aboutVersion: document.getElementById("about-version"),
  btnCheckUpdates: document.getElementById("btn-check-updates"),
  updateCheckStatus: document.getElementById("update-check-status"),
  setAutoCheckUpdates: document.getElementById("set-auto-check-updates"),
  // Update toast
  updateToast: document.getElementById("update-toast"),
  updateToastMsg: document.getElementById("update-toast-msg"),
  updateInstall: document.getElementById("update-install"),
  updateLater: document.getElementById("update-later"),
  // Settings footer
  setSave: document.getElementById("set-save"),
  setCancel: document.getElementById("set-cancel"),
  btnExport: document.getElementById("btn-export"),
  btnImport: document.getElementById("btn-import"),

  // Onboarding modal (driven by the controller in the Onboarding section
  // further down; individual sub-element lookups happen lazily inside it
  // so missing-on-Lite pieces don't show up here as null refs).
  onboarding: document.getElementById("onboarding"),
  btnReplayOnboarding: document.getElementById("btn-replay-onboarding"),

  contextMenu: document.getElementById("context-menu"),
};

// ---------- Init ----------
init();

async function init() {
  // Strip Team Library tab markup in free build.
  if (!TEAMS_BUILD) {
    document.querySelector('.tab[data-tab="team"]')?.remove();
    document.querySelector('.tab-panel[data-tab="team"]')?.remove();
  }

  state.settings = await invoke("get_settings");
  applyTheme(state.settings.theme);
  applyAccentColor(state.settings.accent_color);
  applyCompact(state.settings.compact);

  // Whitelabel detection. The brand_defaults IPC returns the build-time
  // baked-in server URL + sso_only flag from Settings::default(). A
  // non-empty server_url means scripts/brand.mjs ran a substitution and
  // this is a customer-locked build; the UI hides the URL fields so the
  // user can't accidentally point a branded client at a different server.
  if (TEAMS_BUILD) {
    try {
      state.brandDefaults = await invoke("brand_defaults");
    } catch (err) {
      console.warn("brand_defaults failed", err);
    }
    if (state.brandDefaults?.server_url) {
      document.body.dataset.brandLocked = "true";
      // Populate the read-only "Server: <url>" labels that replace the
      // text inputs in this build. Looked up once; the elements live in
      // the static markup so we don't need to re-render on settings save.
      const teamLabel = document.getElementById("brand-server-url-display");
      if (teamLabel) teamLabel.textContent = state.brandDefaults.server_url;
      const onbLabel = document.getElementById("onboarding-brand-server-url");
      if (onbLabel) onbLabel.textContent = state.brandDefaults.server_url;
    }
  }

  await refresh();
  // Supplier reads state.folders live so new folders show up without a refresh hop.
  attachCombobox(els.editorFolderInput, () => state.folders.map((f) => f.path));
  bindEvents();
  attachTreeRootDropTarget();
  focusSearch();

  // Hotkey re-opened the window: reset launcher state.
  await listen("snipdesk://opened", async () => {
    els.search.value = "";
    state.activeTag = null;
    state.selectedFolder = ALL_FOLDERS;
    await refresh();
    focusSearch();
  });

  await listen("snipdesk://open-editor", async () => {
    closeAllModals();
    openEditor();
  });

  await listen("snipdesk://open-settings", async () => {
    closeAllModals();
    openSettings();
  });

  // Payload may be raw string or { text } - handle both in case the emit shape changes.
  await listen("snipdesk://quick-add", async (event) => {
    closeAllModals();
    const p = event?.payload;
    const text = typeof p === "string" ? p : p?.text ?? "";
    openEditor(null, { body: text });
  });

  if (TEAMS_BUILD) {
    await listen("snipdesk://team-library-updated", async () => {
      // refresh() rebuilds the sidebar AND the snippet list, so the
      // Team Library pseudo-node appears/disappears as the source goes
      // active/inactive, not just when the user is currently viewing
      // the team folder. The previous gated version meant a user on
      // "All snippets" wouldn't see the team node show up when the
      // server delivered its first library snippet.
      await refresh();
    });

    // Background sync engine emits these. Update the status panel +
    // re-render the snippet list because new rows may have arrived.
    await listen("snipdesk://server-sync", async () => {
      await loadServerStatus();
      await refresh();
    });

    // The background loop emits this when the server returns 401 and
    // it wipes the stored token. Refresh the UI so the user sees the
    // login form again instead of a stale "signed in as" line.
    await listen("snipdesk://server-signed-out", async () => {
      await loadServerStatus();
      setStatus("Signed out: server rejected your session. Please sign in again.", "err");
    });

    // Server forced us out because the account is disabled or deleted.
    // Distinct from a routine 401 - the user can't fix it by signing
    // back in, they need to contact an admin. The signed-out event
    // (emitted alongside) handles the UI reset; this listener exists
    // to surface the specific reason.
    await listen("snipdesk://server-account-inactive", async (event) => {
      const reason = typeof event.payload === "string"
        ? event.payload
        : "Your account is no longer active.";
      setStatus(reason, "err");
    });

    // Initial paint when settings opens; load once at boot too so the
    // signed-in state is ready by the time Settings is opened.
    await loadServerStatus();
    // Heartbeat + focus-sync wiring runs once at startup. Both bail
    // immediately when not signed in, so they're cheap when the user
    // is offline / never set up sync.
    startServerHeartbeat();
    attachFocusSync();
  }

  // First-run: open the onboarding modal. Both the explicit
  // first-run event from the Rust side and a boot-time check
  // converge on the same controller; whichever fires first wins
  // and the other is a no-op because the modal is already open.
  if (!TEAMS_BUILD) {
    document.querySelector('#onboarding [data-teams-only="true"]')?.remove();
  }
  if (!state.settings?.onboarding_completed) {
    onboarding.start();
  }
  await listen("snipdesk://first-run", async () => {
    if (state.settings?.onboarding_completed) return;
    onboarding.start();
  });

  // Show the running flavor + version in the About tab.
  try {
    const productName = TEAMS_BUILD ? "SnipDesk" : "SnipDesk Lite";
    els.aboutVersion.textContent = `${productName} v${await getVersion()}`;
  } catch (err) {
    console.warn("failed to read app version", err);
  }

  // Silent update poll on launch. Fire-and-forget so a slow/unreachable
  // network never blocks the UI; failures are console.warn only (the manual
  // "Check for updates" button surfaces errors loudly instead).
  if (state.settings?.auto_check_updates) {
    checkForUpdates({ silent: true });
  }
}

// ---------- Onboarding ----------
//
// First-run tour. Modal lives in #onboarding (see index.html);
// panels are siblings that share .onboarding-panel and the active
// one carries .is-active. The controller below drives transitions,
// signal subscriptions (sign-in poll, hotkey demo), and the
// typing-test logic. Lite skips the signin panel entirely; the
// wage step is always skippable.

const ONBOARDING_TYPING_PHRASE =
  "A time may come soon, when none will return. Then there will be need of valour without renown, " +
  "for none shall remember the deeds that are done in the last defence of your homes. " +
  "Yet the deeds will not be less valiant because they are unpraised.";

function onboardingSteps() {
  const all = ["welcome", "signin", "hotkey", "typing", "wage", "done"];
  return TEAMS_BUILD ? all : all.filter((s) => s !== "signin");
}

const onboarding = {
  steps: [],
  index: 0,
  // Step indexes the user has "visited" (been the active step on at
  // least once). Used by gotoStep() to gate forward jumps: clicking a
  // progress dot ahead of the current step is allowed only when every
  // intermediate step has been visited. Back jumps are always allowed.
  visited: new Set(),
  typing: null, // { startedAt, finishedAt, wpm }
  signinPoll: null,
  hotkeyUnlisten: null,

  async start() {
    if (!els.onboarding) return;
    this.steps = onboardingSteps();
    this.index = 0;
    this.visited = new Set();
    closeAllModals();
    els.onboarding.classList.remove("hidden");
    this.show(this.steps[0]);
  },

  show(step) {
    document.querySelectorAll("#onboarding .onboarding-panel").forEach((p) => {
      p.classList.toggle("is-active", p.dataset.step === step);
    });
    // Mark the current step as visited the moment it becomes active.
    // This is what makes future forward-jumps via dots possible: once
    // a step has been on screen, it counts as visited even if the user
    // navigated away with Back.
    this.visited.add(this.index);
    // Welcome is the first step; nothing to go back to.
    const backBtn = document.querySelector(
      `#onboarding [data-step="${step}"] [data-onboarding-back]`,
    );
    if (backBtn) backBtn.disabled = this.index === 0;
    this.renderProgress();
    if (step === "signin") this.primeSigninPanel();
    if (step === "hotkey") this.primeHotkeyPanel();
    if (step === "typing") this.primeTypingPanel();
    if (step === "wage") this.primeWagePanel();
  },

  renderProgress() {
    const container = document.getElementById("onboarding-progress");
    if (!container) return;
    container.innerHTML = "";
    for (let i = 0; i < this.steps.length; i++) {
      const dot = document.createElement("button");
      dot.type = "button";
      const klass =
        i < this.index ? "dot is-done" : i === this.index ? "dot is-current" : "dot";
      dot.className = klass;
      dot.setAttribute(
        "aria-label",
        `Go to step ${i + 1} of ${this.steps.length}: ${this.steps[i]}`,
      );
      if (i === this.index) dot.setAttribute("aria-current", "step");
      // A dot is clickable when:
      //   - It's the current step (no-op, but not disabled for affordance).
      //   - It's a previous step (back jumps always allowed).
      //   - It's a forward step AND every intermediate step has been visited.
      const reachable = i <= this.index || this.canJumpForwardTo(i);
      if (!reachable) dot.disabled = true;
      dot.addEventListener("click", () => this.gotoStep(i));
      container.appendChild(dot);
    }
  },

  canJumpForwardTo(target) {
    for (let i = this.index; i < target; i++) {
      if (!this.visited.has(i)) return false;
    }
    return true;
  },

  gotoStep(target) {
    if (target === this.index) return;
    if (target < 0 || target >= this.steps.length) return;
    if (target > this.index && !this.canJumpForwardTo(target)) return;
    this.cleanupCurrentStep();
    this.index = target;
    this.show(this.steps[this.index]);
  },

  back() {
    if (this.index === 0) return;
    this.gotoStep(this.index - 1);
  },

  advance() {
    this.cleanupCurrentStep();
    this.visited.add(this.index);
    this.index++;
    if (this.index >= this.steps.length) {
      this.complete();
      return;
    }
    this.show(this.steps[this.index]);
  },

  skip() {
    this.advance();
  },

  cleanupCurrentStep() {
    if (this.signinPoll) {
      clearInterval(this.signinPoll);
      this.signinPoll = null;
    }
    if (this.hotkeyUnlisten) {
      try {
        this.hotkeyUnlisten();
      } catch (_e) {}
      this.hotkeyUnlisten = null;
    }
    // Reset the hotkey panel's button visibility so a Replay starts clean.
    document
      .querySelectorAll('#onboarding [data-step="hotkey"] [data-onboarding-next]')
      .forEach((el) => el.classList.add("hidden"));
    const hideBtn = document.getElementById("onboarding-hotkey-hide");
    if (hideBtn) hideBtn.classList.remove("hidden");
  },

  async primeSigninPanel() {
    if (!TEAMS_BUILD) return;
    // Refresh the sign-in surface from the server's reported methods
    // so the onboarding panel shows password / providers consistent
    // with what's actually configured. Non-blocking: if the fetch
    // fails, the conservative default in renderSignInSurface keeps
    // the panel usable.
    const signinUrl =
      state.settings?.server_url || state.brandDefaults?.server_url || "";
    if (signinUrl) loadServerMethods(signinUrl);
    const urlInput = document.getElementById("onboarding-server-url");
    const oidcBtn = document.getElementById("onboarding-signin-oidc");
    if (urlInput) {
      urlInput.value = state.settings?.server_url || "";
      const refreshOidc = () => {
        if (!oidcBtn) return;
        // Brand-locked builds always have a URL to use, so the button
        // is enabled unconditionally. The vanilla build still gates on
        // the user typing something.
        const haveUrl =
          !!state.brandDefaults?.server_url || !!urlInput.value.trim();
        oidcBtn.disabled = !haveUrl;
      };
      // Avoid stacking listeners across Replays.
      urlInput.oninput = refreshOidc;
      refreshOidc();
    }
    await loadServerStatus();
    // Snapshot whether the user is ALREADY signed in when entering
    // this step. renderSigninStatus uses this to distinguish two
    // cases: the user just completed OIDC and we should auto-advance
    // off the panel (transition false -> true), versus the user
    // navigated Back to a step they already cleared and we should
    // leave the panel visible so they can read it and click
    // Continue themselves (already-true on entry).
    this.signinWasSignedInOnEntry = !!state.serverStatus?.signed_in;
    this.renderSigninStatus();
    this.signinPoll = setInterval(async () => {
      await loadServerStatus();
      this.renderSigninStatus();
    }, 1500);
  },

  renderSigninStatus() {
    const status = document.getElementById("onboarding-signin-status");
    const next = document.querySelector(
      '#onboarding [data-step="signin"] [data-onboarding-next]',
    );
    if (state.serverStatus?.signed_in) {
      // serverStatus carries the user under `.user` (UserDto with
      // display_name + email + role). The previous code reached
      // for serverStatus.display_name / .email directly, which are
      // always undefined, and fell through to the literal sentinel.
      const u = state.serverStatus.user;
      const display = u?.display_name || u?.email || "(signed in)";
      if (status) status.textContent = `Signed in as ${display}.`;
      if (next) next.removeAttribute("disabled");

      // Auto-advance off the signin panel once OIDC completes. The
      // entry-snapshot in primeSigninPanel ensures we ONLY advance on
      // the false -> true transition: a user navigating Back to this
      // step while already signed in stays put (signinWasSignedInOnEntry
      // == true), so they can read the confirmation and click Continue
      // themselves. The 800 ms delay lets the "Signed in as <name>"
      // message paint and register before the panel changes.
      if (!this.signinWasSignedInOnEntry && !this.signinAdvancing) {
        this.signinAdvancing = true;
        setTimeout(() => {
          // Guard against advancing the wrong step if the user clicked
          // Skip / Back / a dot in the 800 ms window.
          if (this.steps[this.index] === "signin") {
            this.advance();
          }
          this.signinAdvancing = false;
        }, 800);
      }
    } else {
      if (status) status.textContent = "Waiting for sign-in...";
      if (next) next.setAttribute("disabled", "");
    }
  },

  async startOidc() {
    if (!TEAMS_BUILD) return;
    const urlInput = document.getElementById("onboarding-server-url");
    const status = document.getElementById("onboarding-signin-status");
    // Whitelabel builds hide the URL input and pre-fill from
    // brandDefaults; fall back to that so the user can't be stuck on a
    // disabled button just because the input is collapsed.
    const url =
      (urlInput && urlInput.value.trim()) || state.brandDefaults?.server_url || "";
    if (!url) return;
    // Persist the URL so the OIDC handshake + later sync paths can
    // pick it up via the existing settings.server_url channel.
    try {
      const updated = { ...state.settings, server_url: url };
      state.settings = await invoke("update_settings", { newSettings: updated });
    } catch (err) {
      console.warn("onboarding: server URL save failed", err);
      if (status) status.textContent = "Couldn't save server URL. Try again.";
      return;
    }
    try {
      const startUrl = await invoke("server_oidc_start_url", { serverUrl: url });
      const { open: openExternal } = await import("@tauri-apps/plugin-shell");
      await openExternal(startUrl);
      if (status) status.textContent = "Browser opened. Finish signing in there.";
    } catch (err) {
      console.warn("onboarding: oidc start failed", err);
      if (status) {
        status.textContent =
          "Couldn't open browser. Open Settings -> Team Library to sign in manually.";
      }
    }
  },

  // Manual fallback for when the browser doesn't auto-launch the
  // deep link (Chrome / Edge prompts that the user dismissed, an
  // antivirus stripping the scheme, a whitelabel build whose
  // custom scheme isn't yet wired through the server's allowlist,
  // etc.). The user copies the token from the success page in
  // their browser and pastes here; we validate via the same
  // server_oidc_paste_token IPC the Settings panel uses, then let
  // the next signin poll pick up the now-signed-in state.
  async usePastedToken() {
    const tokenInput = document.getElementById("onboarding-paste-token");
    const errEl = document.getElementById("onboarding-paste-token-error");
    const urlInput = document.getElementById("onboarding-server-url");
    if (errEl) errEl.classList.add("hidden");
    const token = (tokenInput?.value || "").trim();
    if (!token) {
      if (errEl) {
        errEl.textContent = "Paste your sign-in token first.";
        errEl.classList.remove("hidden");
      }
      return;
    }
    // Whitelabel: URL input is hidden; fall back to the baked default.
    const url =
      (urlInput?.value || "").trim() || state.brandDefaults?.server_url || "";
    if (!url) {
      if (errEl) {
        errEl.textContent = "Enter the server URL above before pasting a token.";
        errEl.classList.remove("hidden");
      }
      return;
    }
    // Persist the URL so paste_token has a server to bind the
    // token to. Idempotent if Sign-in-with-Google already ran.
    try {
      const updated = { ...state.settings, server_url: url };
      state.settings = await invoke("update_settings", { newSettings: updated });
    } catch (err) {
      console.warn("onboarding: server URL save failed", err);
    }
    try {
      await invoke("server_oidc_paste_token", { token });
      if (tokenInput) tokenInput.value = "";
      // Immediate refresh + render so Continue lights up without
      // waiting for the next 1.5s poll tick.
      await loadServerStatus();
      this.renderSigninStatus();
    } catch (err) {
      console.warn("onboarding: paste token failed", err);
      if (errEl) {
        errEl.textContent =
          "Couldn't validate that token: " + (err?.message || String(err));
        errEl.classList.remove("hidden");
      }
    }
  },

  primeHotkeyPanel() {
    const label = document.getElementById("onboarding-hotkey-label");
    if (label) label.textContent = formatHotkey(state.settings?.hotkey);
    const status = document.getElementById("onboarding-hotkey-status");
    if (status) status.textContent = "";
  },

  async hideAndTryHotkey() {
    const status = document.getElementById("onboarding-hotkey-status");
    const hideBtn = document.getElementById("onboarding-hotkey-hide");
    const next = document.querySelector(
      '#onboarding [data-step="hotkey"] [data-onboarding-next]',
    );
    if (status) status.textContent = `Waiting for ${formatHotkey(state.settings?.hotkey)}...`;
    // Subscribe BEFORE hiding so we never miss the re-open event.
    try {
      this.hotkeyUnlisten = await listen("snipdesk://opened", () => {
        if (status) status.textContent = "Got it. That's how you'll summon SnipDesk anytime.";
        if (hideBtn) hideBtn.classList.add("hidden");
        if (next) next.classList.remove("hidden");
      });
    } catch (err) {
      console.warn("onboarding: failed to subscribe to opened event", err);
    }
    try {
      await invoke("hide_window");
    } catch (err) {
      console.warn("onboarding: hide_window failed", err);
      if (status) {
        status.textContent =
          "Couldn't hide the window automatically. Press your hotkey anyway, then click Continue.";
      }
      if (next) next.classList.remove("hidden");
    }
  },

  primeTypingPanel() {
    const phraseEl = document.getElementById("onboarding-typing-phrase");
    const input = document.getElementById("onboarding-typing-input");
    if (!phraseEl || !input) return;
    phraseEl.innerHTML = "";
    for (const ch of ONBOARDING_TYPING_PHRASE) {
      const span = document.createElement("span");
      span.className = "ch";
      span.textContent = ch;
      phraseEl.appendChild(span);
    }
    // Mark the very first char as the cursor.
    const first = phraseEl.querySelector(".ch");
    if (first) first.classList.add("cursor");
    input.value = "";
    this.typing = { startedAt: null, finishedAt: null, wpm: null };
    document.getElementById("onboarding-typing-result").classList.add("hidden");
    document.getElementById("onboarding-typing-use").classList.add("hidden");
    document.getElementById("onboarding-typing-restart").classList.add("hidden");
    setTimeout(() => input.focus(), 50);
  },

  onTypingInput() {
    const phraseEl = document.getElementById("onboarding-typing-phrase");
    const input = document.getElementById("onboarding-typing-input");
    if (!phraseEl || !input || !this.typing) return;
    const typed = input.value;
    if (typed.length > 0 && this.typing.startedAt === null) {
      this.typing.startedAt = Date.now();
    }
    const spans = phraseEl.querySelectorAll(".ch");
    spans.forEach((s, i) => {
      s.classList.remove("ok", "bad", "cursor");
      if (i < typed.length) {
        s.classList.add(typed[i] === ONBOARDING_TYPING_PHRASE[i] ? "ok" : "bad");
      } else if (i === typed.length) {
        s.classList.add("cursor");
      }
    });
    if (typed === ONBOARDING_TYPING_PHRASE && !this.typing.finishedAt) {
      this.typing.finishedAt = Date.now();
      const seconds = (this.typing.finishedAt - this.typing.startedAt) / 1000;
      const words = ONBOARDING_TYPING_PHRASE.trim().split(/\s+/).length;
      const wpm = Math.max(1, Math.round((words * 60) / seconds));
      this.typing.wpm = wpm;
      const result = document.getElementById("onboarding-typing-result");
      result.innerHTML = `<strong>${wpm} WPM</strong> over ${seconds.toFixed(1)}s.`;
      result.classList.remove("hidden");
      document.getElementById("onboarding-typing-use").classList.remove("hidden");
      document.getElementById("onboarding-typing-restart").classList.remove("hidden");
      input.blur();
    }
  },

  async useTypingResult() {
    if (!this.typing || this.typing.wpm == null) return;
    await this.saveProfile({ wpm: this.typing.wpm });
    this.advance();
  },

  primeWagePanel() {
    const wageInput = document.getElementById("onboarding-wage");
    const currSel = document.getElementById("onboarding-currency");
    if (!wageInput || !currSel) return;
    wageInput.value =
      state.settings?.hourly_wage && state.settings.hourly_wage > 0
        ? String(state.settings.hourly_wage)
        : "";
    if (currSel.options.length === 0) {
      const codes = [
        "AUD", "USD", "EUR", "GBP", "CAD", "NZD", "JPY", "CHF", "INR", "SGD",
        "HKD", "ZAR", "BRL", "MXN", "KRW", "SEK", "NOK", "DKK", "PLN", "CZK",
        "TRY", "AED", "CNY", "THB", "IDR", "PHP",
      ];
      for (const c of codes) {
        const o = document.createElement("option");
        o.value = c;
        o.textContent = c;
        currSel.appendChild(o);
      }
    }
    const existing = state.settings?.wage_currency;
    if (existing && Array.from(currSel.options).some((o) => o.value === existing)) {
      currSel.value = existing;
    } else {
      currSel.value = guessCurrencyFromLocale();
    }
  },

  async saveWage() {
    const wageInput = document.getElementById("onboarding-wage");
    const currSel = document.getElementById("onboarding-currency");
    const wage = parseFloat(wageInput?.value);
    const currency = currSel?.value;
    const args = {};
    if (Number.isFinite(wage) && wage > 0) args.hourly_wage = wage;
    if (currency) args.currency = currency;
    if (Object.keys(args).length > 0) await this.saveProfile(args);
    this.advance();
  },

  async saveProfile(args) {
    // Always reflect into local settings; the Settings UI uses
    // these fields directly on the Savings tab regardless of build
    // flavor.
    try {
      const updated = { ...state.settings };
      if (typeof args.wpm === "number") updated.typing_speed_wpm = args.wpm;
      if (typeof args.hourly_wage === "number") updated.hourly_wage = args.hourly_wage;
      if (typeof args.currency === "string") updated.wage_currency = args.currency;
      state.settings = await invoke("update_settings", { newSettings: updated });
    } catch (err) {
      console.warn("onboarding: local settings save failed", err);
    }
    // Mirror to the server when Teams + signed in so the dashboard's
    // per-user override picks it up. Silent failure: the local save
    // is the source of truth for the desktop client; server is a
    // nice-to-have for the org dashboard.
    if (TEAMS_BUILD && state.serverStatus?.signed_in) {
      try {
        await invoke("server_update_profile", { args });
      } catch (err) {
        console.warn("onboarding: server profile push failed", err);
      }
    }
  },

  async complete() {
    if (els.onboarding) els.onboarding.classList.add("hidden");
    try {
      const updated = { ...state.settings, onboarding_completed: true };
      state.settings = await invoke("update_settings", { newSettings: updated });
    } catch (err) {
      console.warn("onboarding: complete-write failed", err);
    }
  },

  async replay() {
    try {
      const updated = { ...state.settings, onboarding_completed: false };
      state.settings = await invoke("update_settings", { newSettings: updated });
    } catch (err) {
      console.warn("onboarding: replay-flag write failed", err);
    }
    await this.start();
  },
};

// Used by the onboarding wage panel + (future) any other UI that
// wants a sensible currency default. Mirrors the locale map the
// server's stats page uses; ports it here so the desktop client
// doesn't have to round-trip the server just to guess.
function guessCurrencyFromLocale() {
  const localeMap = {
    AU: "AUD", US: "USD", GB: "GBP", DE: "EUR", FR: "EUR", IT: "EUR", ES: "EUR",
    NL: "EUR", AT: "EUR", BE: "EUR", IE: "EUR", PT: "EUR", FI: "EUR", GR: "EUR",
    JP: "JPY", CA: "CAD", NZ: "NZD", CH: "CHF", IN: "INR", SG: "SGD", HK: "HKD",
    ZA: "ZAR", BR: "BRL", MX: "MXN", KR: "KRW", SE: "SEK", NO: "NOK", DK: "DKK",
    PL: "PLN", CZ: "CZK", TR: "TRY", AE: "AED", CN: "CNY", TH: "THB", ID: "IDR",
    PH: "PHP",
  };
  const lang = (navigator.language || "").toUpperCase();
  const parts = lang.split(/[-_]/);
  const region = parts.length > 1 ? parts[1] : "";
  return (region && localeMap[region]) || "AUD";
}

// ---------- Auto-update ----------
// In-flight guard so overlapping checks (launch poll + manual click) don't
// double-prompt or double-download.
let updateState = { checking: false, installing: false, pending: null };

async function checkForUpdates({ silent }) {
  if (updateState.checking || updateState.installing) return;
  updateState.checking = true;
  if (!silent) {
    els.btnCheckUpdates.disabled = true;
    els.updateCheckStatus.textContent = "Checking...";
    els.updateCheckStatus.className = "update-check-status";
  }
  try {
    const update = await checkUpdate();
    if (update) {
      updateState.pending = update;
      if (!silent) {
        els.updateCheckStatus.textContent = `Version ${update.version} available.`;
        els.updateCheckStatus.className = "update-check-status ok";
      }
      showUpdateToast(update.version);
    } else if (!silent) {
      els.updateCheckStatus.textContent = "You're on the latest version.";
      els.updateCheckStatus.className = "update-check-status ok";
    }
  } catch (err) {
    // Network unreachable / no release / bad signature config.
    if (silent) {
      console.warn("update check failed", err);
    } else {
      els.updateCheckStatus.textContent = `Update check failed: ${err}`;
      els.updateCheckStatus.className = "update-check-status err";
    }
  } finally {
    updateState.checking = false;
    if (!silent) els.btnCheckUpdates.disabled = false;
  }
}

function showUpdateToast(version) {
  els.updateToastMsg.textContent = `SnipDesk ${version} is available.`;
  els.updateInstall.disabled = false;
  els.updateInstall.textContent = "Install and restart";
  els.updateToast.classList.remove("hidden");
}

function dismissUpdateToast() {
  els.updateToast.classList.add("hidden");
}

async function installPendingUpdate() {
  const update = updateState.pending;
  if (!update || updateState.installing) return;
  updateState.installing = true;
  els.updateInstall.disabled = true;
  els.updateLater.disabled = true;

  let downloaded = 0;
  let total = 0;
  try {
    await update.downloadAndInstall((event) => {
      switch (event.event) {
        case "Started":
          total = event.data.contentLength ?? 0;
          els.updateToastMsg.textContent = "Downloading update...";
          break;
        case "Progress":
          downloaded += event.data.chunkLength ?? 0;
          els.updateToastMsg.textContent = total
            ? `Downloading... ${formatBytes(downloaded)} / ${formatBytes(total)}`
            : `Downloading... ${formatBytes(downloaded)}`;
          break;
        case "Finished":
          els.updateToastMsg.textContent = "Installing...";
          break;
      }
    });
    // Installed - relaunch into the new version.
    await relaunch();
  } catch (err) {
    updateState.installing = false;
    els.updateLater.disabled = false;
    els.updateInstall.disabled = false;
    els.updateInstall.textContent = "Retry";
    els.updateToastMsg.textContent = `Update failed: ${err}`;
    console.warn("update install failed", err);
  }
}

function formatBytes(n) {
  if (!n || n < 1024) return `${n || 0} B`;
  const units = ["KB", "MB", "GB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}

// ---------- Theme / compact ----------
function applyTheme(theme) {
  let resolved = theme || "dark";
  if (resolved === "system") {
    const darkPref =
      window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;
    resolved = darkPref ? "dark" : "light";
  }
  document.documentElement.setAttribute("data-theme", resolved);
}

// Returns lowercase 6-digit hex ("#4c9aff") or null. Accepts hex (3 or 6 digits,
// optional #), rgb()/rgba() (alpha ignored), and bare "r, g, b" triples. Backend
// stores hex only; <input type="color"> requires 6-digit hex.
function normalizeAccent(raw) {
  if (raw == null) return null;
  const s = String(raw).trim().toLowerCase();
  if (!s) return null;

  // Hex - 3 or 6 digits, with or without leading #.
  const hexMatch = s.match(/^#?([0-9a-f]{3}|[0-9a-f]{6})$/);
  if (hexMatch) {
    let h = hexMatch[1];
    if (h.length === 3) {
      h = h
        .split("")
        .map((c) => c + c)
        .join("");
    }
    return `#${h}`;
  }

  // rgb() / rgba() / bare triples.
  const rgbMatch = s.match(
    /^(?:rgba?\s*\(\s*)?(\d{1,3})\s*[,\s]\s*(\d{1,3})\s*[,\s]\s*(\d{1,3})(?:\s*[,\s][\d.]+)?\s*\)?$/
  );
  if (rgbMatch) {
    const r = clamp255(rgbMatch[1]);
    const g = clamp255(rgbMatch[2]);
    const b = clamp255(rgbMatch[3]);
    if (r == null || g == null || b == null) return null;
    const toHex = (n) => n.toString(16).padStart(2, "0");
    return `#${toHex(r)}${toHex(g)}${toHex(b)}`;
  }

  return null;
}

function clamp255(str) {
  const n = parseInt(str, 10);
  if (!Number.isFinite(n) || n < 0 || n > 255) return null;
  return n;
}

// --accent-2 derives from --accent via color-mix in styles.css, so setting
// --accent alone repaints both shades. Invalid/blank removes the override.
function applyAccentColor(raw) {
  const hex = normalizeAccent(raw);
  if (hex) {
    document.documentElement.style.setProperty("--accent", hex);
  } else {
    document.documentElement.style.removeProperty("--accent");
  }
}

// Hex of the currently-painted --accent. Used to seed the swatch when there's
// no override so it opens on the theme's accent, not a random default.
function readComputedAccentHex() {
  try {
    const v = getComputedStyle(document.documentElement)
      .getPropertyValue("--accent")
      .trim();
    return normalizeAccent(v) || "#4c9aff";
  } catch (_) {
    return "#4c9aff";
  }
}

function updateAccentPreview(raw) {
  const hex = normalizeAccent(raw);
  if (hex) {
    els.accentPreview.style.background = hex;
    els.accentPreview.style.boxShadow = `0 0 0 2px ${hex}40`;
  } else {
    els.accentPreview.style.background = "var(--accent)";
    els.accentPreview.style.boxShadow =
      "0 0 0 2px color-mix(in srgb, var(--accent) 25%, transparent)";
  }
}

function applyCompact(compact) {
  document.body.classList.toggle("compact", !!compact);
}

function closeAllModals() {
  els.editor.classList.add("hidden");
  els.varPrompt.classList.add("hidden");
  els.linkPrompt.classList.add("hidden");
  els.settings.classList.add("hidden");
  els.dupWarn.classList.add("hidden");
  state.pendingLinkInsert = null;
  hideContextMenu();
}

function formatHotkey(hk) {
  if (!hk) return "Alt+Space";
  return hk.replace(/CommandOrControl/gi, "Ctrl").replace(/CmdOrCtrl/gi, "Ctrl");
}

function focusSearch() {
  requestAnimationFrame(() => {
    els.search.focus();
    els.search.select();
  });
}

// ---------- Data ----------

/// Filter team snippets the same way the backend filters personal ones.
/// Mirrors db::list's folder semantics so a search for "Billing" matches
/// both `Billing` and `Billing/Refunds`.
function filterTeamSnippetsLocal(teamSnippets, folder, search, tagFilter) {
  const q = (search || "").trim().toLowerCase();
  const wantsRoot = folder === ROOT_FOLDER;
  const folderPrefix = folder && folder !== ALL_FOLDERS && !wantsRoot ? folder : null;
  return (teamSnippets || []).filter((s) => {
    // Folder gate
    if (wantsRoot) {
      if (s.folder_path && s.folder_path !== "") return false;
    } else if (folderPrefix) {
      const fp = s.folder_path || "";
      if (fp !== folderPrefix && !fp.startsWith(folderPrefix + "/")) return false;
    }
    // Tag gate
    if (tagFilter && !((s.tags || []).map((t) => String(t).toLowerCase()).includes(String(tagFilter).toLowerCase()))) {
      return false;
    }
    // Search gate
    if (q) {
      const hay =
        (s.title || "").toLowerCase() + " " +
        (s.body || "").toLowerCase() + " " +
        (s.tags || []).join(" ").toLowerCase();
      if (!hay.includes(q)) return false;
    }
    return true;
  });
}

/// Combine two already-sorted lists into one re-sorted list under the
/// same sort mode. Each list came back from the backend in `sort`
/// order; merging without re-sort would clump team snippets at the
/// end, which reads as "team snippets are second-class". Re-sorting
/// in JS is cheap at the volumes involved (hundreds of rows max).
function mergeSorted(personal, team, sort) {
  const all = personal.concat(team);
  if (sort === "alphabetical" || sort === "Alphabetical") {
    all.sort((a, b) => (a.title || "").localeCompare(b.title || "", undefined, { sensitivity: "base" }));
  } else {
    // Usage sort: most-used first. Team snippets carry usage_count = 0
    // (we don't track use counts on shared content), so they end up
    // after a user's actively-used personal snippets - which is the
    // right ordering.
    all.sort((a, b) => {
      const u = (b.usage_count || 0) - (a.usage_count || 0);
      if (u !== 0) return u;
      return (b.updated_at || 0) - (a.updated_at || 0);
    });
  }
  return all;
}

/// Merge team folder paths into the folder tree. A team snippet's
/// `folder_path` either matches an existing user folder (which then
/// gets a `has_team` marker for the cloud-glyph badge) or creates a
/// brand-new folder node. Ancestors of a team folder also gain the
/// marker so the cloud propagates up the tree the way snippet counts
/// do. The folder rows we synthesize here carry `count = 0` because
/// the existing count field is "personal snippets in this folder"
/// only - mixing the team count in would mislead the rename / delete
/// folder dialogs that quote it.
function mergeTeamFoldersIntoTree(personalFolders, teamSnippets) {
  // Same gates as refresh()'s teamPromise: ignore the cached team
  // table when the user is signed out OR has the show-inline toggle
  // off, so the folder tree doesn't surface ghost team folders from
  // a previous session or shared folders they explicitly hid.
  const inlineToggle = state.settings?.show_team_snippets_inline !== false;
  if (
    !TEAMS_BUILD ||
    !state.serverStatus?.signed_in ||
    !inlineToggle ||
    (teamSnippets || []).length === 0
  ) {
    return personalFolders;
  }
  const byPath = new Map();
  for (const f of personalFolders) {
    byPath.set(f.path, { ...f, has_team: false });
  }
  for (const s of teamSnippets) {
    const path = s.folder_path;
    if (!path || path === "") continue;
    const segments = path.split("/").filter(Boolean);
    let acc = "";
    for (const seg of segments) {
      acc = acc ? `${acc}/${seg}` : seg;
      const existing = byPath.get(acc);
      if (existing) {
        existing.has_team = true;
      } else {
        byPath.set(acc, {
          path: acc,
          has_snippets: false,
          count: 0,
          has_team: true,
        });
      }
    }
  }
  return Array.from(byPath.values()).sort((a, b) => a.path.localeCompare(b.path));
}

async function refresh() {
  try {
    const sort = sortModeFromSettings();

    // Team folder uses a separate backend table and command. list_team_snippets
    // doesn't accept a query arg, so search is filtered client-side. Tags/folders
    // still come from the user's own snippets.
    if (TEAMS_BUILD && state.selectedFolder === TEAM_FOLDER) {
      const [teamSnippets, tags, folders, allSnippets] = await Promise.all([
        invoke("list_team_snippets"),
        invoke("list_tags"),
        invoke("list_folders"),
        invoke("list_snippets", { query: null, tag: null, folder: null, sort }),
      ]);
      const q = (els.search.value || "").trim().toLowerCase();
      const filtered = q
        ? (teamSnippets || []).filter((s) => {
            const hay =
              (s.title || "").toLowerCase() +
              " " +
              (s.body || "").toLowerCase() +
              " " +
              (s.tags || []).join(" ").toLowerCase();
            return hay.includes(q);
          })
        : teamSnippets || [];
      state.snippets = filtered;
      state.tags = tags || [];
      state.folders = mergeTeamFoldersIntoTree(folders || [], teamSnippets || []);
      state.allSnippets = allSnippets || [];
    } else {
      // Non-team views: personal + team snippets co-exist when the
      // user is signed in to a server AND the show_team_snippets_inline
      // setting is on (default true). Team rows come from a separate
      // backend command (list_team_snippets has no folder/tag/search
      // arg), so we filter them client-side to match the same selector
      // as personal. Identical folder names collide naturally - both
      // sources land in the same bucket - which is the desired UX:
      // a team "Billing" folder merges with a user's "Billing" rather
      // than appearing twice.
      //
      // Gated on serverStatus.signed_in: stale team_snippets rows from
      // a previous session shouldn't leak into the list of a now-
      // signed-out user. Logout already wipes team_snippets via
      // reset_sync_metadata, but the gate is the belt-and-suspenders.
      const inlineToggle = state.settings?.show_team_snippets_inline !== false;
      const includeTeam =
        TEAMS_BUILD && Boolean(state.serverStatus?.signed_in) && inlineToggle;
      const teamPromise = includeTeam
        ? invoke("list_team_snippets").catch(() => [])
        : Promise.resolve([]);
      const [snippets, tags, folders, allSnippets, teamSnippets] = await Promise.all([
        invoke("list_snippets", {
          query: els.search.value || null,
          tag: state.activeTag,
          folder: state.selectedFolder,
          sort,
        }),
        invoke("list_tags"),
        invoke("list_folders"),
        invoke("list_snippets", { query: null, tag: null, folder: null, sort }),
        teamPromise,
      ]);
      const teamFiltered = filterTeamSnippetsLocal(
        teamSnippets || [],
        state.selectedFolder,
        els.search.value || "",
        state.activeTag,
      );
      state.snippets = mergeSorted(snippets || [], teamFiltered, sort);
      state.tags = tags || [];
      state.folders = mergeTeamFoldersIntoTree(folders || [], teamSnippets || []);
      state.allSnippets = allSnippets || [];
    }

    if (state.selectedIndex >= state.snippets.length) {
      state.selectedIndex = Math.max(0, state.snippets.length - 1);
    }
    reconcileSelectionAfterRefresh();
    renderTags();
    renderFolders();
    renderList();
    renderPreview();
    renderSavings();
    updateFolderDatalist();
    // The sidebar always renders. It used to auto-hide when there were zero
    // folders and no team library URL, but that left users with no visible
    // way to add a folder (the "+" button lives in the sidebar header), so
    // an empty library looked broken. "All snippets" + "+" stay useful even
    // with nothing in the tree.
    els.pane.classList.remove("no-sidebar");
  } catch (err) {
    setStatus(`Error: ${err}`, "err");
  }
}

function sortModeFromSettings() {
  if (!state.settings) return "usage";
  return state.settings.sort_by_usage ? "usage" : "alphabetical";
}

// ---------- Folder tree rendering ----------
// ---------- Drag-and-drop (move snippets, reparent folders) ----------
let dragState = null;

// Clear any lingering drop-target highlight (e.g. a drop that landed off-target).
function clearDropTargets() {
  for (const el of els.folderTree.querySelectorAll(".drop-target")) {
    el.classList.remove("drop-target");
  }
  els.folderTree.classList.remove("root-drop-target");
}

// Treat the empty space below the folder list as a "drop to top level"
// zone. Dropping a nested folder there un-nests it; dropping a snippet
// there moves it to Unfiled. Events on real folder nodes still fire their
// own handlers - we only act when the cursor is over the tree container
// itself (not over a child node).
function isEmptyTreeArea(e) {
  return e.target === els.folderTree;
}
function attachTreeRootDropTarget() {
  const handle = (e) => {
    if (!dragState) return;
    if (!isEmptyTreeArea(e)) {
      els.folderTree.classList.remove("root-drop-target");
      return;
    }
    const valid =
      dragState.type === "snippets" ||
      (dragState.type === "folder" && canReparent(dragState.path, ""));
    e.preventDefault();
    e.dataTransfer.dropEffect = valid ? "move" : "none";
    els.folderTree.classList.toggle("root-drop-target", valid);
  };
  els.folderTree.addEventListener("dragenter", handle);
  els.folderTree.addEventListener("dragover", handle);
  els.folderTree.addEventListener("dragleave", (e) => {
    if (e.target === els.folderTree) {
      els.folderTree.classList.remove("root-drop-target");
    }
  });
  els.folderTree.addEventListener("drop", async (e) => {
    if (!isEmptyTreeArea(e)) return;
    e.preventDefault();
    els.folderTree.classList.remove("root-drop-target");
    const ds = dragState;
    dragState = null;
    if (!ds) return;
    if (ds.type === "snippets") {
      await bulkMoveToFolder(ds.ids, "");
    } else if (ds.type === "folder" && canReparent(ds.path, "")) {
      await reparentFolder(ds.path, "");
    }
  });
}

// Can `srcPath` be reparented under `destFolder` ("" = root)?
function canReparent(srcPath, destFolder) {
  if (destFolder === srcPath) return false; // onto itself
  if (destFolder.startsWith(srcPath + "/")) return false; // into its own descendant
  const currentParent = srcPath.includes("/")
    ? srcPath.slice(0, srcPath.lastIndexOf("/"))
    : "";
  if (destFolder === currentParent) return false; // already there - no-op
  return true;
}

async function reparentFolder(srcPath, destFolder) {
  if (!canReparent(srcPath, destFolder)) return;
  const base = srcPath.split("/").pop();
  const newPath = destFolder ? `${destFolder}/${base}` : base;
  try {
    await invoke("rename_folder", { args: { old_path: srcPath, new_path: newPath } });
    if (destFolder) state.expandedFolders.add(destFolder);
    if (state.selectedFolder === srcPath) state.selectedFolder = newPath;
    setStatus(`Moved folder to ${newPath}`, "ok");
    await refresh();
  } catch (err) {
    setStatus(`Error: ${err}`, "err");
  }
}

// Wire a sidebar node as a drop target. `targetPath` is "" for Unfiled/root.
//
// Both dragenter AND dragover must preventDefault for the drop to be accepted;
// some browsers won't update the cursor reliably without dragenter, so we
// register both with identical handling. dropEffect:"none" on invalid targets
// gives a "no-drop" cursor without entirely rejecting the event, which keeps
// the cursor feedback snappy as the user moves between folders.
function attachFolderDropTarget(node, targetPath) {
  const evaluate = (e) => {
    if (!dragState) return;
    const valid = !(
      dragState.type === "folder" && !canReparent(dragState.path, targetPath)
    );
    // Always preventDefault so the drop event can fire; let dropEffect signal
    // the visual state.
    e.preventDefault();
    e.dataTransfer.dropEffect = valid ? "move" : "none";
    node.classList.toggle("drop-target", valid);
  };
  node.addEventListener("dragenter", evaluate);
  node.addEventListener("dragover", evaluate);
  node.addEventListener("dragleave", () => node.classList.remove("drop-target"));
  node.addEventListener("drop", async (e) => {
    e.preventDefault();
    node.classList.remove("drop-target");
    const ds = dragState;
    dragState = null;
    if (!ds) return;
    if (ds.type === "snippets") {
      await bulkMoveToFolder(ds.ids, targetPath);
    } else if (ds.type === "folder") {
      // Silently no-op on invalid drops (drop into self/descendant); the
      // cursor already told the user it wasn't allowed.
      if (canReparent(ds.path, targetPath)) {
        await reparentFolder(ds.path, targetPath);
      }
    }
  });
}

function renderFolders() {
  els.folderTree.innerHTML = "";

  // Pseudo-nodes: All / Unfiled / Team Library.
  const allNode = folderNodeEl(
    null,
    "All snippets",
    state.selectedFolder === ALL_FOLDERS,
    0,
    false,
    false
  );
  allNode.addEventListener("click", () => selectFolder(ALL_FOLDERS));
  els.folderTree.appendChild(allNode);

  if (state.folders.length > 0) {
    const rootNode = folderNodeEl(
      ROOT_FOLDER,
      "Unfiled",
      state.selectedFolder === ROOT_FOLDER,
      0,
      false,
      false
    );
    rootNode.addEventListener("click", () => selectFolder(ROOT_FOLDER));
    // Drop here to unfile a snippet or move a folder to the top level.
    attachFolderDropTarget(rootNode, "");
    els.folderTree.appendChild(rootNode);
  }

  // Team Library pseudo-node. Two paths feed team_snippets:
  //   1. Legacy pull from a public JSON URL (settings.team_library_url)
  //   2. Server-library sync, populated by the snipdesk-server backend
  //      whenever the user is signed in (no URL configured)
  // The node should appear whenever EITHER path is active. Checking
  // settings.team_library_url alone meant signed-in users never saw
  // the section even when the server delivered shared snippets.
  const teamSourceActive =
    TEAMS_BUILD &&
    (Boolean(state.settings?.team_library_url) ||
      Boolean(state.serverStatus?.signed_in));
  if (teamSourceActive) {
    const teamLabel = state.settings.team_library_folder_name || "Team Library";
    const teamNode = folderNodeEl(
      TEAM_FOLDER,
      teamLabel,
      state.selectedFolder === TEAM_FOLDER,
      0,
      false,
      false
    );
    // Cloud glyph distinguishes from local folders.
    const iconSpan = teamNode.querySelector("span:nth-child(2)");
    if (iconSpan) iconSpan.textContent = "☁ ";
    teamNode.addEventListener("click", () => selectFolder(TEAM_FOLDER));
    els.folderTree.appendChild(teamNode);
  }

  // Trash pseudo-folder. Only meaningful when signed in - server-side
  // trash lives on the snipdesk-server, and Lite builds have no
  // tombstone concept beyond "delete locally and that's it".
  // Clicking opens a modal (not selectFolder) because trash content
  // is fetched fresh from the network each time, doesn't live in
  // state.snippets, and the rendering needs its own action buttons.
  if (TEAMS_BUILD && state.serverStatus?.signed_in) {
    const trashNode = folderNodeEl(
      "__trash__",
      "Trash",
      false,
      0,
      false,
      false
    );
    const iconSpan = trashNode.querySelector("span:nth-child(2)");
    if (iconSpan) iconSpan.textContent = "🗑 ";
    trashNode.addEventListener("click", () => openTrashModal());
    els.folderTree.appendChild(trashNode);
  }

  const hasChildren = new Set();
  for (const f of state.folders) {
    const parts = f.path.split("/");
    if (parts.length > 1) {
      hasChildren.add(parts.slice(0, -1).join("/"));
    }
  }

  for (const f of state.folders) {
    const parts = f.path.split("/");
    const depth = parts.length - 1;
    const parent = depth > 0 ? parts.slice(0, -1).join("/") : null;

    if (parent && !isAncestorChainExpanded(f.path)) continue;

    const node = folderNodeEl(
      f.path,
      parts[parts.length - 1],
      state.selectedFolder === f.path,
      depth,
      hasChildren.has(f.path),
      state.expandedFolders.has(f.path),
      f.count
    );
    // Folders that contain team snippets get a small cloud glyph
    // right of the label. Subtle - same visual weight as the snippet-
    // count badge already there.
    if (f.has_team) {
      const cloud = document.createElement("span");
      cloud.className = "folder-cloud";
      cloud.textContent = "☁";
      cloud.title = "Contains shared team snippets";
      // Insert after the folder-label span so it sits between the
      // label and the count badge.
      const labelEl = node.querySelector(".folder-label");
      if (labelEl) labelEl.after(cloud);
      else node.appendChild(cloud);
    }
    // Drag to reparent; drop snippets/folders onto it.
    node.draggable = true;
    node.addEventListener("dragstart", (e) => {
      dragState = { type: "folder", path: f.path };
      e.dataTransfer.effectAllowed = "move";
      e.dataTransfer.setData("text/plain", f.path);
      node.classList.add("dragging");
    });
    node.addEventListener("dragend", () => {
      node.classList.remove("dragging");
      clearDropTargets();
      dragState = null;
    });
    attachFolderDropTarget(node, f.path);
    // Active folder wears .active; companions in the multi-set get .multi-selected.
    if (
      state.selectedFolderPaths.has(f.path) &&
      state.selectedFolder !== f.path
    ) {
      node.classList.add("multi-selected");
    }
    node.addEventListener("click", (e) => {
      if (e.target.classList.contains("folder-caret")) {
        toggleFolderExpanded(f.path);
        e.stopPropagation();
        return;
      }
      handleFolderClick(f.path, e);
    });
    node.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      e.stopPropagation();
      // Right-click within a multi-selection keeps it; otherwise collapse to
      // the clicked folder. Active filter is preserved either way - users
      // often right-click a folder without wanting to navigate to it.
      const isMulti =
        state.selectedFolderPaths.size > 1 &&
        state.selectedFolderPaths.has(f.path);
      if (isMulti) {
        showBulkFolderContextMenu(e.clientX, e.clientY, [
          ...state.selectedFolderPaths,
        ]);
      } else {
        state.selectedFolderPaths = new Set([f.path]);
        state.folderAnchor = f.path;
        renderFolders();
        showFolderContextMenu(e.clientX, e.clientY, f.path);
      }
    });
    els.folderTree.appendChild(node);
  }
}

function isAncestorChainExpanded(path) {
  const parts = path.split("/");
  for (let i = 1; i < parts.length; i++) {
    const ancestor = parts.slice(0, i).join("/");
    if (!state.expandedFolders.has(ancestor)) return false;
  }
  return true;
}

function toggleFolderExpanded(path) {
  if (state.expandedFolders.has(path)) {
    state.expandedFolders.delete(path);
  } else {
    state.expandedFolders.add(path);
  }
  renderFolders();
}

function folderNodeEl(path, label, isActive, depth, hasChildren, expanded, count) {
  const div = document.createElement("div");
  div.className = "folder-node" + (isActive ? " active" : "");
  div.dataset.path = path ?? "";
  div.style.paddingLeft = `${10 + depth * 12}px`;

  const caret = document.createElement("span");
  caret.className = "folder-caret" + (hasChildren ? "" : " empty");
  caret.textContent = hasChildren ? (expanded ? "▾" : "▸") : "";
  div.appendChild(caret);

  const iconSpan = document.createElement("span");
  if (path === null) iconSpan.textContent = "✦ ";
  else if (path === ROOT_FOLDER) iconSpan.textContent = "∘ ";
  else iconSpan.textContent = "📁 ";
  iconSpan.style.opacity = "0.7";
  iconSpan.style.fontSize = "11px";
  div.appendChild(iconSpan);

  const labelSpan = document.createElement("span");
  labelSpan.className = "folder-label";
  labelSpan.textContent = label;
  div.appendChild(labelSpan);

  if (count > 0) {
    const badge = document.createElement("span");
    badge.className = "folder-count";
    badge.textContent = String(count);
    div.appendChild(badge);
  }

  return div;
}

async function selectFolder(path) {
  state.selectedFolder = path;
  state.selectedIndex = 0;
  state.selectedIds = new Set();
  state.anchorIndex = null;
  // Pseudo-nodes don't participate in multi-select; real folders reset to single.
  if (typeof path === "string" && path !== ROOT_FOLDER && path !== TEAM_FOLDER) {
    state.selectedFolderPaths = new Set([path]);
    state.folderAnchor = path;
    const parts = path.split("/");
    for (let i = 1; i < parts.length; i++) {
      state.expandedFolders.add(parts.slice(0, i).join("/"));
    }
  } else {
    state.selectedFolderPaths = new Set();
    state.folderAnchor = null;
  }
  await refresh();
}

// Combobox reads state.folders live; nothing to refresh. Kept for legacy callers.
function updateFolderDatalist() {}

// ---------- Selection helpers ----------
// Collapse to single-select at index `i` and reset the shift-click anchor.
function selectOnly(i) {
  state.selectedIndex = i;
  state.anchorIndex = i;
  state.selectedIds = new Set();
  const s = state.snippets[i];
  if (s) state.selectedIds.add(s.id);
  renderList();
  renderPreview();
}

// Explorer/Finder semantics: plain = single, Ctrl = toggle, Shift = range from anchor.
// Primary (selectedIndex) always follows the click - drives preview + right-click target.
function handleSnippetClick(i, ev) {
  const s = state.snippets[i];
  if (!s) return;

  if (ev.shiftKey) {
    const anchor = state.anchorIndex ?? state.selectedIndex ?? i;
    const lo = Math.min(anchor, i);
    const hi = Math.max(anchor, i);
    state.selectedIds = new Set();
    for (let k = lo; k <= hi; k++) {
      const snip = state.snippets[k];
      if (snip) state.selectedIds.add(snip.id);
    }
    state.selectedIndex = i;
    // Anchor preserved - shift-click range refinement (A, shift-Z, shift-Y to shrink).
  } else if (ev.ctrlKey || ev.metaKey) {
    if (state.selectedIds.has(s.id) && state.selectedIds.size > 1) {
      state.selectedIds.delete(s.id);
      // Primary deselected - bump onto another selected row so preview isn't blank.
      if (i === state.selectedIndex) {
        const fallbackIdx = state.snippets.findIndex((x) =>
          state.selectedIds.has(x.id)
        );
        if (fallbackIdx >= 0) state.selectedIndex = fallbackIdx;
      }
    } else {
      state.selectedIds.add(s.id);
      state.selectedIndex = i;
      state.anchorIndex = i;
    }
  } else {
    state.selectedIds = new Set([s.id]);
    state.selectedIndex = i;
    state.anchorIndex = i;
  }

  renderList();
  renderPreview();
}

// Shift+Arrow equivalent of Shift-click.
function extendSelectionTo(i) {
  const s = state.snippets[i];
  if (!s) return;
  const anchor = state.anchorIndex ?? state.selectedIndex ?? i;
  const lo = Math.min(anchor, i);
  const hi = Math.max(anchor, i);
  state.selectedIds = new Set();
  for (let k = lo; k <= hi; k++) {
    const snip = state.snippets[k];
    if (snip) state.selectedIds.add(snip.id);
  }
  state.selectedIndex = i;
  renderList();
  renderPreview();
}

// Drop selectedIds that no longer exist after refresh(); fall back to primary row.
function reconcileSelectionAfterRefresh() {
  for (const id of [...state.selectedIds]) {
    if (!state.snippets.some((s) => s.id === id)) state.selectedIds.delete(id);
  }
  if (state.selectedIds.size === 0) {
    const s = state.snippets[state.selectedIndex];
    if (s) state.selectedIds.add(s.id);
  }
  if (state.anchorIndex != null && state.anchorIndex >= state.snippets.length) {
    state.anchorIndex = null;
  }
  // Drop folder multi-select entries for deleted folders.
  const liveFolderPaths = new Set((state.folders || []).map((f) => f.path));
  for (const p of [...state.selectedFolderPaths]) {
    if (!liveFolderPaths.has(p)) state.selectedFolderPaths.delete(p);
  }
  if (state.folderAnchor && !liveFolderPaths.has(state.folderAnchor)) {
    state.folderAnchor = null;
  }
}

// ---------- Folder multi-select helpers ----------
// Visible real folders in render order. Mirrors renderFolders() visibility:
// a folder is only visible when every ancestor is expanded. Used for Shift-click ranges.
function getVisibleFolderPaths() {
  const out = [];
  for (const f of state.folders) {
    const parts = f.path.split("/");
    if (parts.length > 1) {
      let allExpanded = true;
      for (let i = 1; i < parts.length; i++) {
        if (!state.expandedFolders.has(parts.slice(0, i).join("/"))) {
          allExpanded = false;
          break;
        }
      }
      if (!allExpanded) continue;
    }
    out.push(f.path);
  }
  return out;
}

// Caller is responsible for re-rendering.
function selectOnlyFolder(path) {
  state.selectedFolderPaths = new Set([path]);
  state.folderAnchor = path;
}

// Same semantics as handleSnippetClick. Ctrl/Shift only affect the multi-set;
// active filter (selectedFolder) is unchanged. Pseudo-nodes always plain-click.
function handleFolderClick(path, ev) {
  const isRealFolder =
    typeof path === "string" &&
    path !== ROOT_FOLDER &&
    path !== TEAM_FOLDER;
  if (!isRealFolder) {
    selectFolder(path);
    return;
  }
  if (ev.shiftKey) {
    const visible = getVisibleFolderPaths();
    const anchor = state.folderAnchor ?? state.selectedFolder;
    const anchorIdx = visible.indexOf(anchor);
    const clickedIdx = visible.indexOf(path);
    if (anchorIdx < 0 || clickedIdx < 0) {
      // Stale anchor (ancestor collapsed, different kind) - degrade to ctrl-add.
      state.selectedFolderPaths.add(path);
    } else {
      const lo = Math.min(anchorIdx, clickedIdx);
      const hi = Math.max(anchorIdx, clickedIdx);
      state.selectedFolderPaths = new Set(visible.slice(lo, hi + 1));
    }
    renderFolders();
    return;
  }
  if (ev.ctrlKey || ev.metaKey) {
    if (state.selectedFolderPaths.has(path) && state.selectedFolderPaths.size > 1) {
      state.selectedFolderPaths.delete(path);
    } else {
      state.selectedFolderPaths.add(path);
      state.folderAnchor = path;
    }
    renderFolders();
    return;
  }
  selectOnlyFolder(path);
  selectFolder(path);
}

// mode: "keep" moves snippets to Unfiled, "with" deletes them too.
// Sorted deepest-first so children are cleaned before parents.
async function bulkDeleteFolders(paths, mode) {
  if (paths.length === 0) return;
  const deleteSnippets = mode === "with";
  const confirmed = await confirmModal({
    title: deleteSnippets ? "Delete folders and snippets" : "Delete folders",
    message: deleteSnippets
      ? `Delete ${paths.length} folder(s) AND every snippet inside? This cannot be undone.`
      : `Delete ${paths.length} folder(s)? Snippets inside will be moved to Unfiled.`,
    confirmText: deleteSnippets ? "Delete everything" : "Delete",
    danger: true,
  });
  if (!confirmed) return;
  const sorted = [...paths].sort((a, b) => b.split("/").length - a.split("/").length);
  let ok = 0;
  let fail = 0;
  for (const p of sorted) {
    try {
      await invoke("delete_folder", {
        args: { path: p, delete_snippets: deleteSnippets },
      });
      ok++;
    } catch (err) {
      console.warn("folder delete failed for", p, err);
      fail++;
    }
  }
  if (paths.includes(state.selectedFolder)) {
    state.selectedFolder = ALL_FOLDERS;
  }
  state.selectedFolderPaths = new Set();
  state.folderAnchor = null;
  setStatus(
    `Deleted ${ok} folder${ok === 1 ? "" : "s"}${fail ? ` (${fail} failed)` : ""}`,
    fail ? "err" : "ok"
  );
  await refresh();
}

// Delete-only - rename/new-subfolder don't have multi-folder semantics.
function showBulkFolderContextMenu(x, y, paths) {
  const items = [
    {
      label: `${paths.length} folder${paths.length === 1 ? "" : "s"} selected`,
      disabled: true,
    },
    { separator: true },
    {
      label: `Delete ${paths.length} folder${paths.length === 1 ? "" : "s"} (keep snippets)`,
      action: () => bulkDeleteFolders(paths, "keep"),
    },
    {
      label: `Delete ${paths.length} folder${paths.length === 1 ? "" : "s"} AND snippets`,
      danger: true,
      action: () => bulkDeleteFolders(paths, "with"),
    },
  ];
  showContextMenu(x, y, items);
}

// ---------- Rendering ----------
function renderList() {
  els.list.innerHTML = "";
  if (state.snippets.length === 0) {
    const li = document.createElement("li");
    li.style.color = "var(--text-dim)";
    li.style.fontStyle = "italic";
    li.style.cursor = "default";
    if (state.selectedFolder === TEAM_FOLDER) {
      li.textContent = els.search.value
        ? "No team snippets match your search."
        : "No team snippets yet. Open Settings → Team Library and click 'Sync now' to pull them in.";
    } else {
      li.textContent = els.search.value
        ? "No snippets match your search. Press Ctrl+N to add one."
        : state.selectedFolder
          ? "No snippets in this folder. Press Ctrl+N to add one."
          : "No snippets yet. Press Ctrl+N to add one.";
    }
    els.list.appendChild(li);
    return;
  }

  for (const [i, s] of state.snippets.entries()) {
    const li = document.createElement("li");
    li.dataset.index = String(i);
    li.dataset.snippetId = s.id;
    li.setAttribute("role", "option");
    if (i === state.selectedIndex) li.classList.add("selected");
    // Companion rows wear .multi-selected (subtler band); primary keeps .selected.
    if (state.selectedIds.has(s.id) && i !== state.selectedIndex) {
      li.classList.add("multi-selected");
    }

    const isTeam = typeof s.id === "string" && s.id.startsWith("team:");
    if (isTeam) {
      li.classList.add("team-snippet");
    }

    const title = document.createElement("div");
    title.className = "snip-title";
    // .snip-title uses flex + space-between to push the usage count
    // to the far right, so the cloud + title need to live together
    // inside ONE flex item or the cloud gets ripped to the opposite
    // side of the row. The wrapper span carries the existing
    // `:first-child` ellipsis styling, so the title still truncates
    // cleanly when long.
    const titleHead = document.createElement("span");
    titleHead.className = "snip-title-head";
    if (isTeam) {
      const cloud = document.createElement("span");
      cloud.className = "snip-cloud";
      cloud.textContent = "☁";
      cloud.title = "Shared team snippet";
      titleHead.appendChild(cloud);
    }
    const titleText = document.createElement("span");
    titleText.className = "snip-title-text";
    titleText.textContent = s.title;
    titleHead.appendChild(titleText);
    title.appendChild(titleHead);
    const showUsage = state.settings?.show_usage_count ?? true;
    if (showUsage && s.usage_count > 0) {
      const count = document.createElement("span");
      count.className = "snip-count";
      count.textContent = `${s.usage_count} usages`;
      title.appendChild(count);
    }
    li.appendChild(title);

    const body = document.createElement("div");
    body.className = "snip-body";
    body.textContent = s.body.replace(/\n/g, " | ").slice(0, 140);
    li.appendChild(body);

    if (s.folder_path) {
      const folder = document.createElement("div");
      folder.className = "snip-folder";
      folder.textContent = `📁 ${s.folder_path}`;
      li.appendChild(folder);
    }

    if (s.tags.length > 0) {
      const tags = document.createElement("div");
      tags.className = "snip-tags";
      for (const t of s.tags) {
        const tag = document.createElement("span");
        tag.className = "snip-tag";
        tag.textContent = t;
        tags.appendChild(tag);
      }
      li.appendChild(tags);
    }

    li.addEventListener("click", (ev) => {
      handleSnippetClick(i, ev);
    });
    li.addEventListener("dblclick", () => {
      // Double-click always collapses to single + paste.
      selectOnly(i);
      usePastedSnippet(false);
    });
    li.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      // Stop the empty-area handler on #snippet-list from overriding our menu.
      e.stopPropagation();
      // Right-click within multi-selection keeps it; otherwise collapse to this row.
      if (!state.selectedIds.has(s.id)) {
        selectOnly(i);
      } else {
        state.selectedIndex = i;
        renderList();
        renderPreview();
      }
      showSnippetContextMenu(e.clientX, e.clientY, s);
    });

    // Drag to a folder to move. Team snippets are read-only - not draggable.
    const isTeamSnip = typeof s.id === "string" && s.id.startsWith("team:");
    if (!isTeamSnip) {
      li.draggable = true;
      li.addEventListener("dragstart", (e) => {
        // Drag the whole selection when this row is part of it; else just it.
        const ids =
          state.selectedIds.has(s.id) && state.selectedIds.size > 1
            ? [...state.selectedIds].filter(
                (id) => !(typeof id === "string" && id.startsWith("team:"))
              )
            : [s.id];
        dragState = { type: "snippets", ids };
        e.dataTransfer.effectAllowed = "move";
        e.dataTransfer.setData("text/plain", ids.join(","));
        li.classList.add("dragging");
      });
      li.addEventListener("dragend", () => {
        li.classList.remove("dragging");
        clearDropTargets();
        dragState = null;
      });
    }

    els.list.appendChild(li);
  }

  const sel = els.list.querySelector("li.selected");
  if (sel) sel.scrollIntoView({ block: "nearest" });
}

function renderPreview() {
  const s = state.snippets[state.selectedIndex];
  if (!s) {
    els.preview.innerHTML = '<div class="preview-empty">Select a snippet to preview.</div>';
    return;
  }
  els.preview.innerHTML = "";
  const h = document.createElement("h3");
  h.textContent = s.title;
  els.preview.appendChild(h);

  const meta = document.createElement("div");
  meta.style.color = "var(--text-dim)";
  meta.style.fontSize = "11px";
  meta.style.marginBottom = "10px";
  const varNames = extractVarNames(s.body);
  const pieces = [];
  if (s.folder_path) pieces.push(`Folder: ${s.folder_path}`);
  if (s.tags.length) pieces.push(`Tags: ${s.tags.join(", ")}`);
  pieces.push(`Used ${s.usage_count} time${s.usage_count === 1 ? "" : "s"}`);
  if (varNames.length) pieces.push(`Variables: ${varNames.join(", ")}`);
  if (typeof s.id === "string" && s.id.startsWith("team:")) {
    pieces.push("Team library (read-only)");
  }
  meta.textContent = pieces.join(" | ");
  els.preview.appendChild(meta);

  const body = document.createElement("div");
  body.style.whiteSpace = "pre-wrap";
  const parts = splitBodyForPreview(s.body);
  for (const part of parts) {
    if (part.type === "var") {
      const span = document.createElement("span");
      span.className = "preview-var";
      span.textContent = `{${part.name}}`;
      body.appendChild(span);
    } else {
      body.appendChild(document.createTextNode(part.text));
    }
  }
  els.preview.appendChild(body);
}

function renderTags() {
  els.tagStrip.innerHTML = "";
  if (state.tags.length === 0) return;

  const all = document.createElement("span");
  all.className = "tag-chip" + (state.activeTag === null ? " active" : "");
  all.textContent = "All";
  all.addEventListener("click", () => {
    state.activeTag = null;
    refresh();
  });
  els.tagStrip.appendChild(all);

  for (const t of state.tags) {
    const chip = document.createElement("span");
    chip.className = "tag-chip" + (state.activeTag === t ? " active" : "");
    chip.textContent = t;
    chip.addEventListener("click", () => {
      state.activeTag = state.activeTag === t ? null : t;
      refresh();
    });
    els.tagStrip.appendChild(chip);
  }
}

// ---------- Variable helpers ----------
function extractVarNames(body) {
  const re = /\{([A-Za-z0-9_\-]+)\}/g;
  const out = new Set();
  let m;
  while ((m = re.exec(body)) !== null) out.add(m[1]);
  return [...out];
}

function splitBodyForPreview(body) {
  const re = /\{([A-Za-z0-9_\-]+)\}/g;
  const out = [];
  let last = 0;
  let m;
  while ((m = re.exec(body)) !== null) {
    if (m.index > last) out.push({ type: "text", text: body.slice(last, m.index) });
    out.push({ type: "var", name: m[1] });
    last = m.index + m[0].length;
  }
  if (last < body.length) out.push({ type: "text", text: body.slice(last) });
  return out;
}

// ---------- Use / paste ----------
async function usePastedSnippet(copyOnly) {
  const s = state.snippets[state.selectedIndex];
  if (!s) return;
  const vars = extractVarNames(s.body);
  if (vars.length > 0) {
    state.pendingPaste = { snippet: s, copyOnly };
    await openVarPrompt(s, vars);
    return;
  }
  await executeUse(s, {}, copyOnly);
}

async function executeUse(snippet, variables, copyOnly) {
  try {
    const result = await invoke("use_snippet", {
      args: {
        id: snippet.id,
        variables,
        paste_mode: copyOnly ? "clipboard" : null,
      },
    });
    setStatus(
      result.pasted
        ? `Pasted "${snippet.title}"`
        : `Copied "${snippet.title}" to clipboard`,
      "ok"
    );
    bumpLocalUsage(snippet.id);
    renderSavings();
  } catch (err) {
    setStatus(`Error: ${err}`, "err");
  }
}

function bumpLocalUsage(id) {
  for (const list of [state.snippets, state.allSnippets]) {
    const s = list.find((x) => x.id === id);
    if (s) s.usage_count = (Number(s.usage_count) || 0) + 1;
  }
}

async function openVarPrompt(snippet, vars) {
  els.varFields.innerHTML = "";

  // Team snippets aren't tracked for var history.
  const isTeam = typeof snippet.id === "string" && snippet.id.startsWith("team:");
  let history = {};
  if (!isTeam) {
    try {
      history = await invoke("get_var_history", {
        args: { snippet_id: snippet.id, var_names: vars },
      });
    } catch (err) {
      console.warn("var history lookup failed", err);
    }
  }

  for (const v of vars) {
    const wrap = document.createElement("div");
    wrap.className = "var-field";
    const label = document.createElement("label");
    label.textContent = v;
    const input = document.createElement("input");
    input.type = "text";
    input.dataset.varName = v;
    input.setAttribute("autocomplete", "off");

    const suggestions = history[v] || [];
    if (suggestions.length > 0) input.value = suggestions[0];

    wrap.appendChild(label);
    wrap.appendChild(input);
    els.varFields.appendChild(wrap);

    // attachCombobox moves the input into a wrapper, so wire it after appending.
    if (suggestions.length > 0) {
      attachCombobox(input, () => suggestions);
    }
  }
  els.varPrompt.classList.remove("hidden");
  requestAnimationFrame(() => {
    const first = els.varFields.querySelector("input");
    if (first) {
      first.focus();
      first.select();
    }
  });
}

function closeVarPrompt() {
  els.varPrompt.classList.add("hidden");
  state.pendingPaste = null;
  focusSearch();
}

async function submitVarPrompt() {
  if (!state.pendingPaste) return;
  const inputs = els.varFields.querySelectorAll("input[data-var-name]");
  const variables = {};
  for (const i of inputs) variables[i.dataset.varName] = i.value;
  const { snippet, copyOnly } = state.pendingPaste;
  els.varPrompt.classList.add("hidden");
  state.pendingPaste = null;
  await executeUse(snippet, variables, copyOnly);
}

// ---------- Editor ----------
// `overrides` is used by quick-add-from-selection to seed body without a snippet.
function openEditor(snippet = null, overrides = {}) {
  state.editingId = snippet ? snippet.id : null;
  els.editorTitle.textContent = snippet ? "Edit snippet" : "New snippet";
  els.editorTitleInput.value = overrides.title ?? (snippet ? snippet.title : "");
  els.editorTagsInput.value = snippet ? snippet.tags.join(", ") : "";
  els.editorBodyInput.value = overrides.body ?? (snippet ? snippet.body : "");
  let prefillFolder = "";
  if (snippet?.folder_path) {
    prefillFolder = snippet.folder_path;
  } else if (
    typeof state.selectedFolder === "string" &&
    state.selectedFolder &&
    state.selectedFolder !== ROOT_FOLDER &&
    state.selectedFolder !== TEAM_FOLDER
  ) {
    prefillFolder = state.selectedFolder;
  }
  els.editorFolderInput.value = prefillFolder;
  updateFolderDatalist();
  renderFormatToolbar();
  clearEditorError();
  els.editor.classList.remove("hidden");
  requestAnimationFrame(() => els.editorTitleInput.focus());
}

function showEditorError(msg) {
  els.editorError.textContent = msg;
  els.editorError.classList.remove("hidden");
}

function clearEditorError() {
  els.editorError.textContent = "";
  els.editorError.classList.add("hidden");
}

function closeEditor() {
  els.editor.classList.add("hidden");
  state.editingId = null;
  focusSearch();
}

function renderFormatToolbar() {
  const tb = els.editorFormatToolbar;
  tb.innerHTML = "";
  const rules = state.settings?.format_rules || [];
  for (const rule of rules) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "format-btn";
    btn.textContent = rule.label || "?";
    btn.title = `${rule.prefix}text${rule.suffix}`;
    // Prevent mousedown stealing focus from the textarea - Chromium keeps the
    // :focus-visible ring on the button afterwards (the "Bold always highlighted" bug).
    btn.addEventListener("mousedown", (ev) => {
      ev.preventDefault();
    });
    btn.addEventListener("click", (ev) => {
      ev.preventDefault();
      applyFormatRule(rule);
    });
    tb.appendChild(btn);
  }
}

// Suffix matching "](...)" routes to the URL prompt instead of literal insert.
function isLinkRule(rule) {
  return /\]\s*\([^)]*\)\s*$/.test(rule.suffix || "");
}

function applyFormatRule(rule) {
  const ta = els.editorBodyInput;
  const start = ta.selectionStart ?? ta.value.length;
  const end = ta.selectionEnd ?? ta.value.length;

  if (isLinkRule(rule)) {
    openLinkPrompt(start, end, ta.scrollTop);
    return;
  }

  const prefix = rule.prefix ?? "";
  const suffix = rule.suffix ?? "";

  // Scroll preservation across mutation. Chromium's `ta.value = ...` resets
  // scrollTop, and `focus()` / `setSelectionRange()` scroll the caret into view.
  // Use setRangeText (preserves undo stack), focus with preventScroll, then
  // restore scrollTop as a safety net.
  const prevScrollTop = ta.scrollTop;

  if (start !== end) {
    const selected = ta.value.slice(start, end);
    ta.setRangeText(prefix + selected + suffix, start, end, "end");
  } else {
    // No selection - drop caret between prefix/suffix so typing fills the wrap.
    ta.setRangeText(prefix + suffix, start, end, "start");
    const caret = start + prefix.length;
    ta.selectionStart = caret;
    ta.selectionEnd = caret;
  }

  ta.focus({ preventScroll: true });
  ta.scrollTop = prevScrollTop;
}

// Captures selection + scrollTop so submitLinkPrompt can restore them post-insert.
function openLinkPrompt(start, end, scrollTop) {
  state.pendingLinkInsert = { start, end, scrollTop };
  const ta = els.editorBodyInput;
  const selected = start !== end ? ta.value.slice(start, end) : "";
  els.linkTextInput.value = selected;
  els.linkUrlInput.value = "https://";
  els.linkPrompt.classList.remove("hidden");
  requestAnimationFrame(() => {
    // URL first - users almost always paste it before editing the link text.
    els.linkUrlInput.focus();
    els.linkUrlInput.select();
  });
}

function closeLinkPrompt() {
  els.linkPrompt.classList.add("hidden");
  state.pendingLinkInsert = null;
  els.editorBodyInput.focus({ preventScroll: true });
}

function submitLinkPrompt() {
  const pending = state.pendingLinkInsert;
  if (!pending) {
    closeLinkPrompt();
    return;
  }
  const url = (els.linkUrlInput.value || "").trim();
  if (!url) {
    els.linkUrlInput.focus();
    return;
  }
  // Empty link text defaults to the URL.
  const text = (els.linkTextInput.value || "").trim() || url;

  const ta = els.editorBodyInput;
  const { start, end, scrollTop } = pending;
  const replacement = `[${text}](${url})`;
  ta.setRangeText(replacement, start, end, "end");

  // Close before restoring scroll so focus returns cleanly first.
  closeLinkPrompt();
  ta.scrollTop = scrollTop;
}

async function saveEditor() {
  const title = els.editorTitleInput.value.trim();
  const body = els.editorBodyInput.value;
  const tags = els.editorTagsInput.value
    .split(",")
    .map((t) => t.trim().toLowerCase())
    .filter(Boolean);
  const folderRaw = els.editorFolderInput.value.trim();
  const folder_path = folderRaw ? folderRaw : null;

  if (!title) {
    showEditorError("Title is required.");
    return;
  }
  clearEditorError();

  // Backend excludes the editing id from the match - case-sensitive.
  let conflict = null;
  try {
    conflict = await invoke("check_title_conflict", {
      args: { title, exclude_id: state.editingId },
    });
  } catch (err) {
    console.warn("title conflict check failed", err);
  }
  if (conflict?.conflict && conflict.existing_id) {
    state.pendingDuplicateSave = {
      title,
      body,
      tags,
      folder_path,
      existingId: conflict.existing_id,
    };
    openDuplicateWarning(title, conflict);
    return;
  }

  await doSaveEditor({ title, body, tags, folder_path });
}

async function doSaveEditor({ title, body, tags, folder_path }) {
  try {
    if (state.editingId) {
      await invoke("update_snippet", {
        id: state.editingId,
        input: { title, body, tags, folder_path },
      });
      setStatus("Snippet updated", "ok");
    } else {
      await invoke("create_snippet", { input: { title, body, tags, folder_path } });
      setStatus("Snippet created", "ok");
    }
    closeEditor();
    if (folder_path) {
      const parts = folder_path.split("/").filter(Boolean);
      for (let i = 1; i <= parts.length; i++) {
        state.expandedFolders.add(parts.slice(0, i).join("/"));
      }
    }
    await refresh();
    // Push the create/edit to the server immediately so the change
    // appears on other devices (and in the admin dashboard) without
    // waiting for the next 5-minute background tick.
    syncIfTeams();
  } catch (err) {
    // Surface the failure inside the editor (the main status bar is hidden
    // behind this modal at default window size).
    showEditorError(`Couldn't save snippet: ${err}`);
  }
}

// ---------- Duplicate-title warning ----------
function openDuplicateWarning(title, conflict) {
  const where = conflict?.existing_folder
    ? ` in "${conflict.existing_folder}"`
    : "";
  els.dupWarnMsg.textContent =
    `A snippet titled "${conflict?.existing_title ?? title}"${where} already exists.`;
  els.dupWarn.classList.remove("hidden");
  els.dupWarn.dataset.existingId = conflict?.existing_id ?? "";
}

function closeDuplicateWarning() {
  els.dupWarn.classList.add("hidden");
  els.dupWarn.removeAttribute("data-existing-id");
  state.pendingDuplicateSave = null;
}

async function openExistingConflict() {
  const existingId = els.dupWarn.dataset.existingId;
  closeDuplicateWarning();
  closeEditor();
  if (!existingId) return;
  // Fast path: in-memory lookup first.
  let snippet = state.snippets.find((s) => s.id === existingId);
  if (!snippet) {
    snippet = state.allSnippets.find((s) => s.id === existingId);
  }
  if (!snippet) {
    try {
      snippet = await invoke("get_snippet", { id: existingId });
    } catch (err) {
      setStatus(`Error: ${err}`, "err");
      return;
    }
  }
  if (snippet) openEditor(snippet);
}

// ---------- Delete / duplicate ----------
async function deleteCurrent() {
  const s = state.snippets[state.selectedIndex];
  if (!s) return;
  if (typeof s.id === "string" && s.id.startsWith("team:")) {
    setStatus("Team library snippets are read-only. Edit the team JSON instead.", "err");
    return;
  }
  const ok = await confirmModal({
    title: "Delete snippet",
    message: `Delete "${s.title}"? This cannot be undone.`,
    confirmText: "Delete",
    danger: true,
  });
  if (!ok) return;
  try {
    await invoke("delete_snippet", { id: s.id });
    setStatus("Snippet deleted", "ok");
    await refresh();
    // Push the tombstone to the server now so it shows up in the user's
    // Trash modal immediately, not only after the next 5-min tick.
    syncIfTeams();
  } catch (err) {
    setStatus(`Error: ${err}`, "err");
  }
}

async function duplicateSnippet(id) {
  if (typeof id === "string" && id.startsWith("team:")) {
    // Team → personal library copy (editable).
    try {
      const source = await invoke("get_snippet", { id });
      openEditor(null, { title: `${source.title} (copy)`, body: source.body });
    } catch (err) {
      setStatus(`Error: ${err}`, "err");
    }
    return;
  }
  try {
    const dup = await invoke("duplicate_snippet", { id });
    setStatus(`Duplicated as "${dup.title}"`, "ok");
    await refresh();
    const idx = state.snippets.findIndex((s) => s.id === dup.id);
    if (idx >= 0) {
      state.selectedIndex = idx;
      renderList();
      renderPreview();
    }
  } catch (err) {
    setStatus(`Error: ${err}`, "err");
  }
}

// ---------- Context menu ----------
function showSnippetContextMenu(x, y, snippet) {
  // Single-row right-click already collapses upstream, so isMulti is reliable here.
  const isMulti = state.selectedIds.size > 1 && state.selectedIds.has(snippet.id);
  if (isMulti) {
    showBulkContextMenu(x, y, [...state.selectedIds]);
    return;
  }

  const isTeam = typeof snippet.id === "string" && snippet.id.startsWith("team:");
  const items = [
    { label: "Paste", action: () => usePastedSnippet(false) },
    { label: "Copy to clipboard", action: () => usePastedSnippet(true) },
    { separator: true },
  ];
  if (isTeam) {
    items.push({ label: "Copy to my library", action: () => duplicateSnippet(snippet.id) });
  } else {
    items.push({ label: "Edit...", action: () => openEditor(snippet) });
    items.push({ label: "Move to folder...", action: () => moveSnippetToFolder(snippet) });
    items.push({ label: "Duplicate", action: () => duplicateSnippet(snippet.id) });
    items.push({ separator: true });
    items.push({ label: "Delete", danger: true, action: () => deleteCurrent() });
  }
  showContextMenu(x, y, items);
}

// Skips team-library IDs (read-only) and surfaces the skip count.
function showBulkContextMenu(x, y, ids) {
  const count = ids.length;
  const localIds = ids.filter((id) => !(typeof id === "string" && id.startsWith("team:")));
  const teamCount = count - localIds.length;
  const label = (verb) => {
    const base = `${verb} ${localIds.length} snippet${localIds.length === 1 ? "" : "s"}`;
    return teamCount > 0 ? `${base} (skip ${teamCount} team)` : base;
  };
  const items = [
    {
      label: `${count} snippet${count === 1 ? "" : "s"} selected`,
      disabled: true,
    },
    { separator: true },
    { label: label("Move"), action: () => bulkMoveToFolder(localIds) },
    { label: label("Tag"), action: () => bulkEditTags(localIds) },
    { label: label("Duplicate"), action: () => bulkDuplicate(localIds) },
    { separator: true },
    { label: label("Delete"), danger: true, action: () => bulkDelete(localIds) },
  ];
  showContextMenu(x, y, items);
}

// ---------- Bulk operations ----------
// Hydrates via get_snippet because state.snippets is the filtered view -
// selected ids may not be present when search/folder filters are active.
async function forEachBulk(ids, action) {
  let ok = 0;
  let fail = 0;
  for (const id of ids) {
    try {
      const s = await invoke("get_snippet", { id });
      if (!s) {
        fail++;
        continue;
      }
      await action(s);
      ok++;
    } catch (err) {
      console.warn("bulk action failed for", id, err);
      fail++;
    }
  }
  return { ok, fail };
}

async function bulkDelete(ids) {
  if (ids.length === 0) {
    setStatus("Nothing to delete - all selected snippets are read-only.", "err");
    return;
  }
  const confirmed = await confirmModal({
    title: "Delete snippets",
    message: `Delete ${ids.length} snippet${ids.length === 1 ? "" : "s"}? This cannot be undone.`,
    confirmText: "Delete",
    danger: true,
  });
  if (!confirmed) return;
  let ok = 0;
  let fail = 0;
  for (const id of ids) {
    try {
      await invoke("delete_snippet", { id });
      ok++;
    } catch (err) {
      console.warn("delete failed for", id, err);
      fail++;
    }
  }
  state.selectedIds = new Set();
  state.anchorIndex = null;
  setStatus(
    `Deleted ${ok} snippet${ok === 1 ? "" : "s"}${fail ? ` (${fail} failed)` : ""}`,
    fail ? "err" : "ok"
  );
  await refresh();
  // Bulk deletes also push immediately - same UX rationale as the
  // single-delete path.
  syncIfTeams();
}

// Move a single snippet via the folder picker. Used by the snippet context
// menu and as the drop handler for snippet→folder drag.
async function moveSnippetToFolder(snippet, targetOverride) {
  if (typeof snippet.id === "string" && snippet.id.startsWith("team:")) {
    setStatus("Team library snippets are read-only.", "err");
    return;
  }
  let target;
  if (targetOverride !== undefined) {
    target = targetOverride || null;
  } else {
    const chosen = await chooseFolderPath(snippet.folder_path ?? null);
    if (chosen === undefined) return;
    target = chosen || null;
  }
  if ((snippet.folder_path ?? null) === target) return;
  try {
    await invoke("update_snippet", {
      id: snippet.id,
      input: {
        title: snippet.title,
        body: snippet.body,
        tags: snippet.tags,
        folder_path: target,
      },
    });
    setStatus(target ? `Moved to "${target}"` : "Moved to Unfiled", "ok");
    await refresh();
  } catch (err) {
    setStatus(`Error: ${err}`, "err");
  }
}

async function bulkMoveToFolder(ids, targetOverride) {
  if (ids.length === 0) return;
  let target;
  if (targetOverride !== undefined) {
    target = targetOverride || null;
  } else {
    const chosen = await chooseFolderPath(null);
    if (chosen === undefined) return;
    target = chosen || null;
  }
  const { ok, fail } = await forEachBulk(ids, async (s) => {
    await invoke("update_snippet", {
      id: s.id,
      input: {
        title: s.title,
        body: s.body,
        tags: s.tags,
        folder_path: target,
      },
    });
  });
  setStatus(
    `Moved ${ok}${target ? ` to "${target}"` : " to Unfiled"}${fail ? ` (${fail} failed)` : ""}`,
    fail ? "err" : "ok"
  );
  await refresh();
}

async function bulkEditTags(ids) {
  if (ids.length === 0) return;
  const raw = await textInputModal({
    title: `Edit tags for ${ids.length} snippet${ids.length === 1 ? "" : "s"}`,
    label: "'+tag' adds | '-tag' removes | no prefix replaces all (comma-separated)",
    placeholder: "+urgent, escalation",
    confirmText: "Apply",
  });
  if (raw === null) return;
  const trimmed = raw.trim();
  let mode = "set";
  let listStr = trimmed;
  if (trimmed.startsWith("+")) {
    mode = "add";
    listStr = trimmed.slice(1).trim();
  } else if (trimmed.startsWith("-")) {
    mode = "remove";
    listStr = trimmed.slice(1).trim();
  }
  const tags = listStr
    .split(",")
    .map((t) => t.trim())
    .filter(Boolean);

  const { ok, fail } = await forEachBulk(ids, async (s) => {
    let next;
    if (mode === "set") {
      next = tags;
    } else if (mode === "add") {
      next = s.tags.slice();
      for (const t of tags) if (!next.includes(t)) next.push(t);
    } else {
      next = s.tags.filter((t) => !tags.includes(t));
    }
    await invoke("update_snippet", {
      id: s.id,
      input: {
        title: s.title,
        body: s.body,
        tags: next,
        folder_path: s.folder_path ?? null,
      },
    });
  });
  const verb = mode === "add" ? "Added tags on" : mode === "remove" ? "Removed tags on" : "Set tags on";
  setStatus(`${verb} ${ok} snippet${ok === 1 ? "" : "s"}${fail ? ` (${fail} failed)` : ""}`, fail ? "err" : "ok");
  await refresh();
}

async function bulkDuplicate(ids) {
  if (ids.length === 0) return;
  let ok = 0;
  let fail = 0;
  for (const id of ids) {
    try {
      await invoke("duplicate_snippet", { id });
      ok++;
    } catch (err) {
      console.warn("duplicate failed for", id, err);
      fail++;
    }
  }
  setStatus(
    `Duplicated ${ok} snippet${ok === 1 ? "" : "s"}${fail ? ` (${fail} failed)` : ""}`,
    fail ? "err" : "ok"
  );
  await refresh();
}

async function createNewFolderPrompt() {
  const name = await textInputModal({
    title: "New folder",
    label: "Folder path (use '/' for nesting)",
    placeholder: "Billing/Refunds",
    confirmText: "Create",
  });
  if (!name) return;
  try {
    await invoke("create_folder", { args: { path: name } });
    const parts = name.split("/").filter(Boolean);
    for (let i = 1; i < parts.length; i++) {
      state.expandedFolders.add(parts.slice(0, i).join("/"));
    }
    await refresh();
  } catch (err) {
    setStatus(`Error: ${err}`, "err");
  }
}

function showFolderContextMenu(x, y, folderPath) {
  const items = [
    {
      label: "New subfolder...",
      action: async () => {
        const name = await textInputModal({
          title: "New subfolder",
          label: `Subfolder of "${folderPath}"`,
          confirmText: "Create",
        });
        if (!name) return;
        try {
          await invoke("create_folder", {
            args: { path: `${folderPath}/${name}` },
          });
          state.expandedFolders.add(folderPath);
          await refresh();
        } catch (err) {
          setStatus(`Error: ${err}`, "err");
        }
      },
    },
    {
      label: "Rename...",
      action: async () => {
        const current = folderPath.split("/").pop();
        const next = await textInputModal({
          title: "Rename folder",
          label: `Rename "${folderPath}" to`,
          value: current,
          confirmText: "Rename",
        });
        if (!next || next === current) return;
        const parts = folderPath.split("/");
        parts[parts.length - 1] = next;
        const newPath = parts.join("/");
        try {
          await invoke("rename_folder", {
            args: { old_path: folderPath, new_path: newPath },
          });
          setStatus(`Renamed folder to ${newPath}`, "ok");
          if (state.selectedFolder === folderPath) state.selectedFolder = newPath;
          await refresh();
        } catch (err) {
          setStatus(`Error: ${err}`, "err");
        }
      },
    },
    { separator: true },
    {
      label: "Delete folder (keep snippets)",
      action: async () => {
        const ok = await confirmModal({
          title: "Delete folder",
          message: `Delete folder "${folderPath}"? Snippets inside will be moved to Unfiled.`,
          confirmText: "Delete",
          danger: true,
        });
        if (!ok) return;
        try {
          await invoke("delete_folder", {
            args: { path: folderPath, delete_snippets: false },
          });
          if (state.selectedFolder === folderPath) state.selectedFolder = ALL_FOLDERS;
          await refresh();
        } catch (err) {
          setStatus(`Error: ${err}`, "err");
        }
      },
    },
    {
      label: "Delete folder AND snippets",
      danger: true,
      action: async () => {
        const ok = await confirmModal({
          title: "Delete folder and snippets",
          message: `Delete folder "${folderPath}" AND every snippet inside? This cannot be undone.`,
          confirmText: "Delete everything",
          danger: true,
        });
        if (!ok) return;
        try {
          await invoke("delete_folder", {
            args: { path: folderPath, delete_snippets: true },
          });
          if (state.selectedFolder === folderPath) state.selectedFolder = ALL_FOLDERS;
          await refresh();
        } catch (err) {
          setStatus(`Error: ${err}`, "err");
        }
      },
    },
  ];
  showContextMenu(x, y, items);
}

// ---------- In-app dialogs ----------
// Promise-based replacements for window.prompt/confirm, which are unreliable
// in some webviews (notably macOS WKWebView returns null silently) and ignore
// the app's styling. Built dynamically so they layer above the context menu
// and the editor/settings modals.
function openModal(buildBody) {
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "modal app-modal";
    const card = document.createElement("div");
    card.className = "modal-card dlg-card";
    overlay.appendChild(card);
    document.body.appendChild(overlay);

    let done = false;
    const finish = (value) => {
      if (done) return;
      done = true;
      document.removeEventListener("keydown", onKey, true);
      overlay.remove();
      resolve(value);
    };
    // Capture + stopImmediatePropagation so Escape closes only this dialog and
    // never reaches the app's global Escape (which hides the window).
    const onKey = (e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopImmediatePropagation();
        finish(null);
      }
    };
    document.addEventListener("keydown", onKey, true);
    overlay.addEventListener("mousedown", (e) => {
      if (e.target === overlay) finish(null);
    });

    buildBody(card, finish);
  });
}

function dlgActions(card, buttons) {
  const actions = document.createElement("div");
  actions.className = "modal-actions";
  for (const b of buttons) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = b.className || "btn-secondary";
    btn.textContent = b.label;
    btn.addEventListener("click", b.onClick);
    actions.appendChild(btn);
  }
  card.appendChild(actions);
  return actions;
}

// Single-line text input. Resolves to the trimmed value, or null if cancelled
// or left empty.
function textInputModal({ title, label, value = "", placeholder = "", confirmText = "OK" }) {
  return openModal((card, finish) => {
    const h = document.createElement("h2");
    h.textContent = title;
    card.appendChild(h);

    const lab = document.createElement("label");
    if (label) {
      const span = document.createElement("span");
      span.textContent = label;
      lab.appendChild(span);
    }
    const input = document.createElement("input");
    input.type = "text";
    input.value = value;
    input.placeholder = placeholder;
    input.spellcheck = false;
    input.autocomplete = "off";
    lab.appendChild(input);
    card.appendChild(lab);

    const submit = () => finish(input.value.trim() || null);
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        // Stop the global Enter handler from also pasting the active snippet.
        e.stopPropagation();
        submit();
      }
    });
    dlgActions(card, [
      { label: "Cancel", onClick: () => finish(null) },
      { label: confirmText, className: "btn-primary", onClick: submit },
    ]);
    setTimeout(() => {
      input.focus();
      input.select();
    }, 0);
  });
}

// Confirmation. Resolves true/false.
function confirmModal({ title, message, confirmText = "OK", danger = false }) {
  return openModal((card, finish) => {
    const h = document.createElement("h2");
    h.textContent = title;
    card.appendChild(h);
    const p = document.createElement("p");
    p.className = "dlg-msg";
    p.textContent = message;
    card.appendChild(p);
    const actions = dlgActions(card, [
      { label: "Cancel", onClick: () => finish(false) },
      {
        label: confirmText,
        className: danger ? "btn-danger" : "btn-primary",
        onClick: () => finish(true),
      },
    ]);
    // Focus the confirm button so Enter activates it naturally without
    // catching the global paste-on-Enter handler. Escape still cancels
    // via the overlay's keyhandler.
    setTimeout(() => {
      const confirmBtn = actions.querySelector("button:last-child");
      if (confirmBtn) confirmBtn.focus();
    }, 0);
  });
}

// Folder chooser. Resolves to { path } for an existing folder ("" = Unfiled),
// { create: true } to make a new one, or null if cancelled.
function folderPickerModal({ title = "Move to folder", currentPath = null } = {}) {
  return openModal((card, finish) => {
    const h = document.createElement("h2");
    h.textContent = title;
    card.appendChild(h);

    const list = document.createElement("div");
    list.className = "folder-picker";
    const addItem = (label, value, depth, isCurrent) => {
      const it = document.createElement("div");
      it.className = "folder-picker-item" + (isCurrent ? " current" : "");
      it.style.paddingLeft = `${10 + depth * 14}px`;
      it.textContent = label;
      it.addEventListener("click", () => finish({ path: value }));
      list.appendChild(it);
    };
    addItem("∘ Unfiled (no folder)", "", 0, currentPath == null || currentPath === "");
    for (const f of state.folders || []) {
      const parts = f.path.split("/");
      addItem(`📁 ${parts[parts.length - 1]}`, f.path, parts.length - 1, currentPath === f.path);
    }
    card.appendChild(list);

    const actions = dlgActions(card, [
      { label: "New folder...", onClick: () => finish({ create: true }) },
      { label: "Cancel", onClick: () => finish(null) },
    ]);
    actions.classList.add("between");
  });
}

// Orchestrates the picker + "New folder..." path. Resolves to the chosen
// folder_path ("" = Unfiled), or undefined if cancelled at any step.
async function chooseFolderPath(currentPath = null) {
  const res = await folderPickerModal({ currentPath });
  if (!res) return undefined;
  if (res.create) {
    const name = await textInputModal({
      title: "New folder",
      label: "Folder path (use '/' for nesting)",
      placeholder: "Billing/Refunds",
      confirmText: "Create",
    });
    return name ?? undefined;
  }
  return res.path;
}

// ---------- Hotkey capture ----------
// Convert a regular text input into a "click here and press a key combo"
// field. Lets users set hotkeys by demonstrating them instead of typing
// "Alt+Space" by hand. Output format matches what parse_shortcut accepts
// on the Rust side.
function enableHotkeyCapture(input, { allowClear = false } = {}) {
  input.readOnly = true;
  let originalValue = "";
  const originalPlaceholder = input.placeholder || "";

  input.addEventListener("focus", () => {
    originalValue = input.value;
    input.value = "";
    input.placeholder = "Press a key combination...";
  });

  input.addEventListener("blur", () => {
    // User tabbed away or clicked elsewhere without pressing a key - put
    // back what was there. The "intentionally clear" path goes through
    // Backspace below (only for inputs where blank means "disabled").
    if (!input.value) input.value = originalValue;
    input.placeholder = originalPlaceholder;
  });

  input.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      input.value = originalValue;
      input.blur();
      return;
    }
    if (allowClear && (e.key === "Backspace" || e.key === "Delete")) {
      e.preventDefault();
      e.stopPropagation();
      input.value = "";
      originalValue = "";
      input.blur();
      return;
    }
    // Modifier-only press - keep waiting for the main key.
    if (["Control", "Shift", "Alt", "Meta", "OS"].includes(e.key)) return;

    e.preventDefault();
    e.stopPropagation();

    const keyToken = codeToHotkeyToken(e.code);
    if (!keyToken) return; // unmappable (numpad, etc.) - ignore

    const mods = [];
    if (e.ctrlKey) mods.push("Ctrl");
    if (e.altKey) mods.push("Alt");
    if (e.shiftKey) mods.push("Shift");
    if (e.metaKey) mods.push("Cmd");

    // Function keys are fine alone (F1-F12 are common global hotkeys);
    // everything else needs at least one modifier to avoid registering
    // bare letters as a global hotkey that swallows normal typing.
    const isFunctionKey = /^F\d+$/.test(keyToken);
    if (!isFunctionKey && mods.length === 0) return;

    input.value = [...mods, keyToken].join("+");
    input.blur();
  });
}

function codeToHotkeyToken(code) {
  if (code.startsWith("Key")) return code.slice(3); // KeyA → A
  if (code.startsWith("Digit")) return code.slice(5); // Digit1 → 1
  if (/^F\d+$/.test(code)) return code; // F1 → F1
  const map = {
    Space: "Space",
    Enter: "Enter",
    Tab: "Tab",
    Backspace: "Backspace",
    ArrowUp: "Up",
    ArrowDown: "Down",
    ArrowLeft: "Left",
    ArrowRight: "Right",
  };
  return map[code] || null;
}

function showContextMenu(x, y, items) {
  const menu = els.contextMenu;
  menu.innerHTML = "";
  for (const item of items) {
    if (item.separator) {
      const sep = document.createElement("div");
      sep.className = "context-menu-sep";
      menu.appendChild(sep);
      continue;
    }
    const el = document.createElement("div");
    el.className =
      "context-menu-item" +
      (item.danger ? " danger" : "") +
      (item.disabled ? " disabled" : "");
    el.textContent = item.label;
    if (!item.disabled) {
      el.addEventListener("click", async () => {
        hideContextMenu();
        try {
          await item.action();
        } catch (err) {
          setStatus(`Error: ${err}`, "err");
        }
      });
    }
    menu.appendChild(el);
  }
  menu.classList.remove("hidden");
  menu.style.left = "0px";
  menu.style.top = "0px";
  const rect = menu.getBoundingClientRect();
  const maxX = window.innerWidth - rect.width - 4;
  const maxY = window.innerHeight - rect.height - 4;
  menu.style.left = `${Math.max(2, Math.min(x, maxX))}px`;
  menu.style.top = `${Math.max(2, Math.min(y, maxY))}px`;
}

function hideContextMenu() {
  els.contextMenu.classList.add("hidden");
}

// ---------- Settings: tabbed modal ----------
function activateTab(name) {
  const tabs = els.settings.querySelectorAll(".tab");
  const panels = els.settings.querySelectorAll(".tab-panel");
  tabs.forEach((t) => t.classList.toggle("active", t.dataset.tab === name));
  panels.forEach((p) => p.classList.toggle("active", p.dataset.tab === name));
  // Opening the Team Library tab triggers a fresh methods fetch so
  // the sign-in surface (password / provider buttons / paste-fallback)
  // matches the server's current config. Cheap call - one round-trip
  // to /api/auth/methods, results cached in state.serverMethods.
  if (name === "team" && TEAMS_BUILD) {
    const url = els.setServerUrl?.value?.trim() ||
                state.settings?.server_url ||
                state.brandDefaults?.server_url ||
                "";
    if (url) loadServerMethods(url);
  }
}

function openSettings() {
  const s = state.settings;
  // General
  els.setPasteMode.value = s.paste_mode;
  els.setDelay.value = s.auto_paste_delay_ms;
  els.setSortMode.value = s.sort_by_usage ? "usage" : "alphabetical";
  els.setClose.checked = !!s.close_on_paste;
  els.setAutostart.checked = s.start_with_windows ?? true;
  els.setCloseToTray.checked = s.close_to_tray ?? true;
  els.setMinimizeToTray.checked = s.minimize_to_tray ?? true;
  els.setStartInTray.checked = s.start_in_tray ?? false;
  els.setAutoCheckUpdates.checked = s.auto_check_updates ?? true;
  // Appearance
  els.setTheme.value = s.theme || "dark";
  // Empty accent = theme default, but <input type="color"> can't represent "no
  // color". Seed swatch from computed --accent so it opens on the visible color
  // while the text field stays blank to indicate "no override".
  const currentAccent = s.accent_color || "";
  els.setAccentText.value = currentAccent;
  els.setAccentText.classList.remove("invalid");
  const swatchSeed = normalizeAccent(currentAccent) || readComputedAccentHex();
  els.setAccentColor.value = swatchSeed;
  updateAccentPreview(currentAccent || swatchSeed);
  els.setCompact.checked = !!s.compact;
  els.setShowUsage.checked = s.show_usage_count ?? true;
  els.setHideOnBlur.checked = s.hide_on_blur ?? false;
  els.setAlwaysOnTop.checked = s.always_on_top ?? false;
  // Hotkeys
  els.setHotkey.value = s.hotkey || "";
  els.setQuickAddHotkey.value = s.quick_add_hotkey ?? "";
  if (TEAMS_BUILD) {
    if (els.setShowTeamInline) {
      els.setShowTeamInline.checked = s.show_team_snippets_inline !== false;
    }
  }
  // Working copy so Cancel actually cancels.
  state.editingRules = deepCloneRules(s.format_rules || DEFAULT_FORMAT_RULES);
  renderRuleEditor();
  // Savings
  els.setShowSavings.checked = !!s.show_savings_estimate;
  els.setWpm.value = s.typing_speed_wpm ?? 40;
  els.setWage.value = s.hourly_wage ?? 0;
  els.setCurrency.value = s.wage_currency ?? "$";
  // About
  els.setBackupDays.value = s.backup_retention_days ?? 14;
  els.setLogDays.value = s.log_retention_days ?? 7;
  loadLogPath();
  // Server panel re-load every time settings opens; the background
  // sync may have changed `last_sync` since the last paint.
  loadServerStatus();
  // Always open on the General tab.
  activateTab("general");
  els.settings.classList.remove("hidden");
}

function deepCloneRules(rules) {
  return rules.map((r) => ({
    label: r.label ?? "",
    prefix: r.prefix ?? "",
    suffix: r.suffix ?? "",
  }));
}

function renderRuleEditor() {
  els.ruleRows.innerHTML = "";
  for (const [i, rule] of state.editingRules.entries()) {
    const tr = document.createElement("tr");

    const labelTd = document.createElement("td");
    const labelInput = document.createElement("input");
    labelInput.type = "text";
    labelInput.value = rule.label;
    labelInput.addEventListener("input", () => { rule.label = labelInput.value; });
    labelTd.appendChild(labelInput);
    tr.appendChild(labelTd);

    const prefixTd = document.createElement("td");
    const prefixInput = document.createElement("input");
    prefixInput.type = "text";
    prefixInput.value = rule.prefix;
    prefixInput.addEventListener("input", () => { rule.prefix = prefixInput.value; });
    prefixTd.appendChild(prefixInput);
    tr.appendChild(prefixTd);

    const suffixTd = document.createElement("td");
    const suffixInput = document.createElement("input");
    suffixInput.type = "text";
    suffixInput.value = rule.suffix;
    suffixInput.addEventListener("input", () => { rule.suffix = suffixInput.value; });
    suffixTd.appendChild(suffixInput);
    tr.appendChild(suffixTd);

    const delTd = document.createElement("td");
    const delBtn = document.createElement("button");
    delBtn.type = "button";
    delBtn.className = "rule-del";
    delBtn.textContent = "✕";
    delBtn.title = "Remove rule";
    delBtn.addEventListener("click", () => {
      state.editingRules.splice(i, 1);
      renderRuleEditor();
    });
    delTd.appendChild(delBtn);
    tr.appendChild(delTd);

    els.ruleRows.appendChild(tr);
  }
}

// ---------- Team library UI ----------
function formatRelativeTime(unix) {
  // null = never synced.
  if (!unix || typeof unix !== "number") return "Never";
  const now = Math.floor(Date.now() / 1000);
  const d = Math.max(0, now - unix);
  if (d < 60) return `${d}s ago`;
  if (d < 3600) return `${Math.floor(d / 60)}m ago`;
  if (d < 86400) return `${Math.floor(d / 3600)}h ago`;
  return `${Math.floor(d / 86400)}d ago`;
}

// The legacy team_library_url UI (Last synced / Snippets loaded /
// Last error / Sync now button) was removed as part of the v1
// de-bloat. The underlying status payload + sync command still work
// for anyone who set team_library_url via settings.json before the
// removal; we just don't surface them in the app any more.
//
// renderTeamStatus / loadTeamStatus / syncTeamLibraryNow used to
// drive those status fields. They're gone with the UI.

async function loadLogPath() {
  try {
    const p = await invoke("get_log_path");
    els.logPathDisplay.textContent = p ? `Log file: ${p}` : "";
  } catch (err) {
    els.logPathDisplay.textContent = "";
  }
}

// ---------- Server (Teams personal-snippet sync) ----------
// State machine: load status → render either signed-out or signed-in
// view → handlers flip between them via login / signup / logout / sync.
//
// All commands are Teams-build-only; callers guard with TEAMS_BUILD.

function showServerError(msg) {
  els.serverError.textContent = msg;
  els.serverError.classList.remove("hidden");
}
function clearServerError() {
  els.serverError.textContent = "";
  els.serverError.classList.add("hidden");
}

function formatSyncTimestamp(unixSecs) {
  if (!unixSecs) return "Never";
  const d = new Date(unixSecs * 1000);
  return d.toLocaleString();
}

// ---------- Trash ----------

/// Open the trash modal and fetch the user's server-side tombstones.
/// Content is never cached locally; we always go to the server when
/// the modal opens so a snippet deleted on another device shows up
/// without a separate sync.
async function openTrashModal() {
  if (!TEAMS_BUILD || !els.trashModal) return;
  els.trashList.innerHTML = '<p class="muted">Loading...</p>';
  els.trashModal.classList.remove("hidden");
  try {
    const items = await invoke("server_trash_list");
    renderTrashList(items || []);
  } catch (err) {
    els.trashList.innerHTML = `<p class="muted">Couldn't load trash: ${escapeHtmlBasic(String(err))}</p>`;
  }
}

function closeTrashModal() {
  if (els.trashModal) els.trashModal.classList.add("hidden");
}

function renderTrashList(items) {
  if (!items || items.length === 0) {
    els.trashList.innerHTML = '<p class="muted">Trash is empty.</p>';
    return;
  }
  els.trashList.innerHTML = "";
  for (const item of items) {
    const row = document.createElement("div");
    row.className = "trash-row";
    row.dataset.snippetId = item.id;

    const header = document.createElement("div");
    header.className = "trash-row-head";
    const title = document.createElement("strong");
    title.textContent = item.payload?.title || "(untitled)";
    header.appendChild(title);
    const when = document.createElement("span");
    when.className = "muted small";
    when.textContent = `deleted ${formatRelativeTime(item.deleted_at)}`;
    header.appendChild(when);
    row.appendChild(header);

    const body = document.createElement("div");
    body.className = "trash-row-body";
    const bodyText = (item.payload?.body || "").replace(/\n/g, " | ");
    body.textContent = bodyText.length > 200 ? bodyText.slice(0, 200) + "..." : bodyText;
    row.appendChild(body);

    if (item.payload?.folder_path) {
      const folder = document.createElement("div");
      folder.className = "trash-row-folder muted small";
      folder.textContent = `📁 ${item.payload.folder_path}`;
      row.appendChild(folder);
    }

    const actions = document.createElement("div");
    actions.className = "trash-row-actions";
    const restoreBtn = document.createElement("button");
    restoreBtn.className = "btn-primary small";
    restoreBtn.textContent = "Restore";
    restoreBtn.addEventListener("click", async () => {
      restoreBtn.disabled = true;
      restoreBtn.textContent = "Restoring...";
      try {
        await invoke("server_trash_restore", { id: item.id });
        // Remove the row from the modal optimistically.
        row.remove();
        if (els.trashList.children.length === 0) {
          els.trashList.innerHTML = '<p class="muted">Trash is empty.</p>';
        }
        setStatus(`Restored "${item.payload?.title || "snippet"}".`, "ok");
        // Refresh main list so the restored snippet appears.
        await refresh();
      } catch (err) {
        restoreBtn.disabled = false;
        restoreBtn.textContent = "Restore";
        setStatus(`Restore failed: ${err}`, "err");
      }
    });
    actions.appendChild(restoreBtn);
    row.appendChild(actions);

    els.trashList.appendChild(row);
  }
}

/// Minimal HTML escape for status / error strings that we inject as
/// innerHTML. Avoids depending on the textContent path for these tiny
/// status messages.
function escapeHtmlBasic(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

/// Fire a server sync in the background if we're signed in. Doesn't
/// block the caller - errors are logged but swallowed because the
/// mutation that triggered this sync already succeeded locally; the
/// next regular sync tick will retry the push.
///
/// Called after every local snippet create / edit / delete so the
/// server sees the change immediately rather than waiting up to 5
/// minutes for the background sync. Heartbeat-style ticks live
/// separately (see startServerHeartbeat).
function syncIfTeams() {
  if (!TEAMS_BUILD) return;
  if (!state.serverStatus?.signed_in) return;
  invoke("server_sync_now").catch((err) => {
    console.debug("background sync after mutation failed:", err);
  });
}

/// 60-second heartbeat that flushes any unsynced data + updates the
/// server's last_seen for this user. The AuthUser extractor on the
/// server side bumps last_seen_at on every authenticated request, so
/// any cheap authenticated call gets the side effect; we use the
/// full sync_now because it also pushes pending dirty rows. The
/// alternative (a dedicated /api/heartbeat that does nothing
/// expensive) would let us run faster, but at 60s cadence the cost
/// of a full sync_now is negligible and the bonus is that the user's
/// data stays fresh on the server even when they aren't actively
/// editing.
///
/// Kicked off on app launch and on every successful sign-in.
let serverHeartbeatTimer = null;
function startServerHeartbeat() {
  if (!TEAMS_BUILD) return;
  if (serverHeartbeatTimer) return;
  serverHeartbeatTimer = setInterval(() => {
    if (!state.serverStatus?.signed_in) return;
    // Sneak past the standard background-sync interval (which runs
    // every 5 minutes); this is a "keep state warm" tick, not a
    // full reconciliation. Errors are silent - the background loop
    // will eventually retry.
    invoke("server_sync_now").catch((err) => {
      console.debug("heartbeat sync failed:", err);
    });
  }, 60_000);
}

/// Window-focus listener. Fires a sync the moment the user comes
/// back to the app from another window. Doesn't double-fire with the
/// heartbeat because syncIfTeams is idempotent at the server level
/// (no-op if nothing changed).
function attachFocusSync() {
  if (!TEAMS_BUILD) return;
  window.addEventListener("focus", () => {
    syncIfTeams();
  });
}

async function loadServerStatus() {
  if (!TEAMS_BUILD) return;
  try {
    const st = await invoke("server_status");
    state.serverStatus = st;
    renderServerStatus();
  } catch (err) {
    console.warn("server_status failed", err);
  }
}

/// Fetch /api/auth/methods and update state.serverMethods. Used by
/// renderSignInSurface to decide which fields/buttons to show. Safe
/// to call repeatedly; just refreshes the cache. Failures (network,
/// server not reachable) leave state.serverMethods untouched and
/// renderSignInSurface falls back to its conservative default.
async function loadServerMethods(serverUrl) {
  if (!TEAMS_BUILD) return;
  const url = (serverUrl || "").trim();
  if (!url) return;
  try {
    state.serverMethods = await invoke("server_auth_methods", { serverUrl: url });
    renderSignInSurface();
  } catch (err) {
    // Non-fatal: a brand-new install pointed at an unreachable
    // server should still let the user see the URL field and try
    // again. Log so a real misconfiguration is visible in the
    // console, but don't surface an error in the UI.
    console.warn("server_auth_methods failed", err);
  }
}

/// Show/hide the password section, Google button, "or" divider, and
/// paste-token fallback in the signed-out panel based on what
/// state.serverMethods reports. Called on Team Library tab open, on
/// successful methods fetch, and from renderServerStatus when the
/// signed-out panel is the visible one.
function renderSignInSurface() {
  if (!TEAMS_BUILD) return;
  const methods = state.serverMethods;
  // Conservative default: when we haven't fetched yet (or the fetch
  // failed), show the password section so the user has SOMETHING to
  // act on. Hide the OIDC pieces - they'd just be broken buttons
  // without the server confirming the provider is wired.
  const showPassword = methods ? methods.password.enabled : true;
  const providers = methods ? methods.providers : [];
  const googleProvider = providers.find((p) => p.id === "google") || null;
  const otherProviders = providers.filter((p) => p.id !== "google");
  const hasAnyProvider = providers.length > 0;

  if (els.serverPasswordSection) {
    els.serverPasswordSection.classList.toggle("hidden", !showPassword);
  }
  if (els.btnServerOidc) {
    // Google button is static markup with the brand-compliant SVG.
    // Show it when the server reports google as configured; stash
    // the start_url on a dataset attribute so doServerOidcStart can
    // read it without re-querying state.serverMethods.
    els.btnServerOidc.classList.toggle("hidden", !googleProvider);
    if (googleProvider) {
      els.btnServerOidc.dataset.startUrl = googleProvider.start_url || "";
    }
  }
  // Dynamic buttons for any non-Google provider (currently Keycloak,
  // and any future IdP the server adds). Rendered inside the same
  // container as the Google button, after it.
  renderDynamicProviderButtons(otherProviders);
  if (els.serverAuthDivider) {
    // The divider only earns its place when both sides have content.
    els.serverAuthDivider.classList.toggle("hidden", !(showPassword && hasAnyProvider));
  }
  if (els.serverPasteFallback) {
    els.serverPasteFallback.classList.toggle("hidden", !hasAnyProvider);
  }
}

/// Render a button per non-Google provider into #server-providers.
/// Re-running this is idempotent: existing dynamic buttons are
/// stripped first, then re-rendered from the current provider list.
/// The Google button (with its branded SVG) stays in place because
/// it's static markup; only buttons we own get torn down.
function renderDynamicProviderButtons(providers) {
  if (!els.serverProviders) return;
  // Strip prior dynamic buttons. data-dynamic="1" tags ones we
  // created; the static Google button has no such attribute.
  els.serverProviders
    .querySelectorAll('[data-dynamic="1"]')
    .forEach((el) => el.remove());
  for (const p of providers) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "btn-secondary server-oidc-btn";
    btn.dataset.dynamic = "1";
    btn.dataset.providerId = p.id;
    btn.dataset.startUrl = p.start_url || "";
    btn.textContent = p.display_name || "Sign in with SSO";
    btn.addEventListener("click", () => doServerOidcStart(p));
    els.serverProviders.appendChild(btn);
  }
}

function renderServerStatus() {
  const st = state.serverStatus;
  if (!st) return;
  if (st.signed_in && st.user) {
    els.serverSignedOut.classList.add("hidden");
    els.serverSignedIn.classList.remove("hidden");
    els.serverUserName.textContent = st.user.display_name;
    els.serverUserEmail.textContent = st.user.email;
    els.serverUserRole.textContent = st.user.role;
    els.serverUrlDisplay.textContent = st.server_url;
    // The user's per-server wpm/wage/currency overrides are now
    // synced from the Savings tab on every saveSettings(); no
    // dedicated inputs to re-populate here.
    els.serverLastSync.textContent = st.last_sync
      ? formatSyncTimestamp(st.last_sync.at)
      : "Never";
    if (st.last_sync) {
      const ls = st.last_sync;
      els.serverLastResult.textContent =
        `${ls.pushed} pushed | ${ls.pulled} pulled` +
        (ls.errors ? ` | ${ls.errors} errors` : "");
      els.serverSyncDetail.style.display = "";
    } else {
      els.serverSyncDetail.style.display = "none";
    }
  } else {
    els.serverSignedIn.classList.add("hidden");
    els.serverSignedOut.classList.remove("hidden");
    // Pre-fill server URL from settings (last successful sign-in).
    if (st.server_url && !els.setServerUrl.value) {
      els.setServerUrl.value = st.server_url;
    }
    // Keep the sign-in surface in sync with the configured methods.
    // Conservative fallback applies if we haven't fetched yet.
    renderSignInSurface();
  }
}

async function doServerLogin() {
  if (!TEAMS_BUILD) return;
  const server_url = els.setServerUrl.value.trim();
  const email = els.setServerEmail.value.trim();
  const password = els.setServerPassword.value;
  if (!server_url || !email || !password) {
    showServerError("Fill in server URL, email, and password.");
    return;
  }
  clearServerError();
  els.btnServerLogin.disabled = true;
  els.btnServerLogin.textContent = "Signing in...";
  try {
    await invoke("server_login", {
      args: { server_url, email, password },
    });
    els.setServerPassword.value = "";
    await afterSignedIn();
  } catch (err) {
    showServerError(String(err));
  } finally {
    els.btnServerLogin.disabled = false;
    els.btnServerLogin.textContent = "Sign in";
  }
}

/// Open the server's OIDC start URL in the system browser. The
/// server returns us via snipdesk://auth?token=... which the deep-
/// link handler in Rust picks up. The B fallback (paste-token form)
/// covers the case where the OS didn't claim the URL scheme.
async function doServerOidcStart(provider) {
  if (!TEAMS_BUILD) return;
  // `provider` is the entry from /api/auth/methods (passed by the
  // dynamic-button click handler) OR undefined when invoked from the
  // hardcoded Google button. In the Google case we synthesise a
  // provider object from the dataset stamped during renderSignInSurface.
  if (!provider && els.btnServerOidc && els.btnServerOidc.dataset.startUrl) {
    provider = {
      id: "google",
      display_name: "Sign in with Google",
      start_url: els.btnServerOidc.dataset.startUrl,
    };
  }
  // Whitelabel builds hide the URL input; fall back to the build-time
  // baked default so the SSO flow has something to talk to.
  const server_url =
    els.setServerUrl.value.trim() || state.brandDefaults?.server_url || "";
  if (!server_url) {
    showServerError("Enter the server URL before signing in.");
    return;
  }
  const isGoogle = provider && provider.id === "google";
  const triggerButton = isGoogle
    ? els.btnServerOidc
    : els.serverProviders?.querySelector(
        `[data-provider-id="${provider?.id}"]`,
      );
  const originalLabel = triggerButton
    ? isGoogle
      ? triggerButton.querySelector(".btn-google-label")?.textContent
      : triggerButton.textContent
    : null;
  clearServerError();
  if (triggerButton) triggerButton.disabled = true;
  if (isGoogle) {
    const oidcLabel = els.btnServerOidc.querySelector(".btn-google-label");
    if (oidcLabel) oidcLabel.textContent = "Opening browser...";
  } else if (triggerButton) {
    triggerButton.textContent = "Opening browser...";
  }
  let startUrl = null;
  try {
    startUrl = await invoke("server_oidc_start_url", {
      serverUrl: server_url,
      startPath: provider?.start_url || null,
    });
    // tauri-plugin-shell exports `open` (not `openUrl`). It hands off
    // to the OS default browser. If it throws - permissions misconfig,
    // plugin not loaded in this build, etc. - we DON'T silently fall
    // back to window.open (which doesn't work from inside a Tauri
    // webview anyway). Instead we surface the URL so the user can
    // paste it into their browser manually.
    const { open: openExternal } = await import("@tauri-apps/plugin-shell");
    await openExternal(startUrl);
    const label = provider?.display_name?.replace(/^Sign in with /, "") || "SSO";
    setStatus(
      `Browser opened. Finish signing in with ${label} there - SnipDesk will pick up automatically.`,
      "ok",
    );
  } catch (err) {
    console.error("Sign in failed:", err);
    if (startUrl) {
      // Best-fallback we can do without external tooling: copy the URL
      // to the clipboard so the user can paste it into their browser.
      try {
        await navigator.clipboard.writeText(startUrl);
        showServerError(
          `Couldn't auto-open the browser. The sign-in URL has been copied to your clipboard - paste it into a browser tab to continue.`,
        );
      } catch (_clipErr) {
        showServerError(`Couldn't open the browser: ${err}. URL: ${startUrl}`);
      }
    } else {
      showServerError(String(err));
    }
  } finally {
    if (triggerButton) triggerButton.disabled = false;
    if (isGoogle) {
      const lbl = els.btnServerOidc.querySelector(".btn-google-label");
      if (lbl) lbl.textContent = originalLabel || "Sign in with Google";
    } else if (triggerButton && originalLabel != null) {
      triggerButton.textContent = originalLabel;
    }
  }
}

/// Manual fallback: user pastes the token from the browser landing
/// page into a field, we validate via /api/me and persist it just
/// like the deep-link path would have. Used when the OS didn't claim
/// snipdesk:// for any reason (AV interference, corp-locked Windows,
/// etc.).
async function doServerPasteToken() {
  if (!TEAMS_BUILD) return;
  const token = (els.setServerPasteToken.value || "").trim();
  if (!token) {
    showServerError("Paste the sign-in token from the browser first.");
    return;
  }
  // Make sure the server URL is saved before we try; the IPC needs
  // it to know which keychain entry to write.
  const server_url = els.setServerUrl.value.trim();
  if (!server_url) {
    showServerError("Enter the server URL above first.");
    return;
  }
  clearServerError();
  els.btnServerPasteToken.disabled = true;
  els.btnServerPasteToken.textContent = "Validating...";
  try {
    // server_oidc_start_url persists the URL too, so calling it
    // (and ignoring the returned URL) ensures settings are aligned
    // even if the user pasted without ever clicking "Sign in with
    // Google" first.
    await invoke("server_oidc_start_url", { serverUrl: server_url });
    await invoke("server_oidc_paste_token", { token });
    els.setServerPasteToken.value = "";
    await afterSignedIn();
    setStatus("Signed in.", "ok");
  } catch (err) {
    showServerError(String(err));
  } finally {
    els.btnServerPasteToken.disabled = false;
    els.btnServerPasteToken.textContent = "Use this token";
  }
}

async function doServerSignup() {
  if (!TEAMS_BUILD) return;
  const server_url = els.setServerUrl.value.trim();
  const email = els.setServerEmail.value.trim();
  const password = els.setServerPassword.value;
  if (!server_url || !email || !password) {
    showServerError("Fill in server URL, email, and password.");
    return;
  }
  // The signup flow needs a display name; reuse the local part of the
  // email as the seed and let the user edit later via account settings
  // (when we add them). Keeps onboarding to one form.
  const display_name = await textInputModal({
    title: "Display name",
    label: "What should other team members see?",
    value: email.split("@")[0],
    confirmText: "Create account",
  });
  if (!display_name) return;
  clearServerError();
  els.btnServerSignup.disabled = true;
  els.btnServerSignup.textContent = "Creating...";
  try {
    await invoke("server_signup", {
      args: { server_url, email, password, display_name },
    });
    els.setServerPassword.value = "";
    await afterSignedIn();
    // First-login offer: upload existing local snippets if there are any.
    const totalLocal = state.allSnippets?.length ?? 0;
    if (totalLocal > 0) {
      const ok = await confirmModal({
        title: "Upload existing snippets?",
        message: `Upload your ${totalLocal} existing local snippet${totalLocal === 1 ? "" : "s"} to the server? They'll sync across devices going forward.`,
        confirmText: "Upload",
      });
      if (ok) {
        try {
          const pushed = await invoke("server_migrate_local_snippets");
          setStatus(`Uploaded ${pushed} snippet${pushed === 1 ? "" : "s"} to the server.`, "ok");
        } catch (err) {
          setStatus(`Migration failed: ${err}`, "err");
        }
      }
    }
  } catch (err) {
    showServerError(String(err));
  } finally {
    els.btnServerSignup.disabled = false;
    els.btnServerSignup.textContent = "Create account";
  }
}

async function afterSignedIn() {
  await loadServerStatus();
  // Refresh local settings cache so the new server_url shows in the form.
  try {
    state.settings = await invoke("get_settings");
  } catch (err) {
    console.warn("get_settings after login failed", err);
  }
  // Trigger an immediate sync so the user sees changes right away.
  try {
    await invoke("server_sync_now");
    await loadServerStatus();
    await refresh();
  } catch (err) {
    console.warn("initial sync after sign-in failed", err);
  }
}

async function doServerLogout() {
  if (!TEAMS_BUILD) return;
  const ok = await confirmModal({
    title: "Sign out",
    message:
      "Sign out of the snippet server? Your local snippets stay; sync stops until you sign in again.",
    confirmText: "Sign out",
  });
  if (!ok) return;
  try {
    await invoke("server_logout");
    await loadServerStatus();
    await refresh();
  } catch (err) {
    setStatus(`Sign-out failed: ${err}`, "err");
  }
}

async function doServerSyncNow() {
  if (!TEAMS_BUILD) return;
  els.btnServerSync.disabled = true;
  els.btnServerSync.textContent = "Syncing...";
  try {
    await invoke("server_sync_now");
    await loadServerStatus();
    await refresh();
    setStatus("Synced.", "ok");
  } catch (err) {
    setStatus(`Sync failed: ${err}`, "err");
  } finally {
    els.btnServerSync.disabled = false;
    els.btnServerSync.textContent = "Sync now";
  }
}

// Mirror the user's local Savings-tab wpm/wage/currency to the
// server's per-user override (PATCH /api/me). Called from
// saveSettings after the local save succeeds. Only runs when
// Teams + signed in; silent no-op otherwise.
//
// Field translation:
//   - wpm always pushes (always meaningful for the dashboard estimate).
//   - hourly_wage > 0 pushes, else null (clearing the override =
//     dashboard uses its server-wide default; local 0 means "don't
//     show money in the footer").
//   - currency only pushes when it parses as a 3-letter ISO code
//     (USD, AUD, EUR ...). The local Savings field accepts a free-
//     form symbol like "$" for the footer, which isn't ISO; in that
//     case we pass null so the server uses its dashboard default.
async function syncProfileToServer(settings) {
  if (!TEAMS_BUILD) return;
  if (!state.serverStatus?.signed_in) return;
  const wpm = Number.isFinite(settings.typing_speed_wpm)
    ? settings.typing_speed_wpm
    : null;
  const wage = Number.isFinite(settings.hourly_wage) && settings.hourly_wage > 0
    ? settings.hourly_wage
    : null;
  const currRaw = (settings.wage_currency || "").trim().toUpperCase();
  const currency = /^[A-Z]{3}$/.test(currRaw) ? currRaw : null;
  try {
    await invoke("server_update_profile", {
      args: { wpm, hourly_wage: wage, currency },
    });
  } catch (err) {
    // Non-fatal: local settings save already succeeded; the dashboard
    // override just won't reflect this change. Logged for diagnosis.
    console.warn("server profile sync failed:", err);
  }
}

// ---------- Settings save ----------
// Shared with "Sync now" so the parsing isn't duplicated.
function collectSettingsForSave() {
  return {
    // General
    paste_mode: els.setPasteMode.value,
    auto_paste_delay_ms: parseInt(els.setDelay.value, 10) || 120,
    sort_by_usage: els.setSortMode.value === "usage",
    close_on_paste: els.setClose.checked,
    start_with_windows: els.setAutostart.checked,
    close_to_tray: els.setCloseToTray.checked,
    minimize_to_tray: els.setMinimizeToTray.checked,
    start_in_tray: els.setStartInTray.checked,
    auto_check_updates: els.setAutoCheckUpdates.checked,
    // Appearance
    theme: els.setTheme.value,
    // Unparseable accent → "no override" (input is flagged invalid on-type).
    accent_color: normalizeAccent(els.setAccentText.value) || "",
    compact: els.setCompact.checked,
    show_usage_count: els.setShowUsage.checked,
    hide_on_blur: els.setHideOnBlur.checked,
    always_on_top: els.setAlwaysOnTop.checked,
    // Hotkeys
    hotkey: els.setHotkey.value.trim(),
    quick_add_hotkey: els.setQuickAddHotkey.value.trim(),
    // Legacy team_library_url fields no longer have a UI; we
    // round-trip whatever was last persisted so update_settings
    // doesn't blank them for users who configured the legacy
    // pull-from-JSON path before the v1 UI cleanup. The underlying
    // sync code still honours these if set.
    team_library_url: state.settings?.team_library_url ?? "",
    team_library_sync_interval_mins:
      state.settings?.team_library_sync_interval_mins ?? 60,
    team_library_folder_name:
      state.settings?.team_library_folder_name ?? "Team Library",
    team_library_sync_on_startup:
      state.settings?.team_library_sync_on_startup ?? true,
    show_team_snippets_inline:
      TEAMS_BUILD && els.setShowTeamInline
        ? els.setShowTeamInline.checked
        : (state.settings?.show_team_snippets_inline ?? true),
    // Server URL is managed by login/logout flows (not directly editable
    // from the form), but we round-trip the current value so update_settings
    // doesn't reset it to the default "".
    server_url: state.settings?.server_url ?? "",
    // prefer_sso_signin lost its UI when the SSO toggle was removed
    // (the /api/auth/methods endpoint drives sign-in visibility now);
    // round-trip the last persisted value to keep update_settings
    // happy.
    prefer_sso_signin: state.settings?.prefer_sso_signin ?? false,
    // Editor rules
    format_rules: state.editingRules
      .map((r) => ({
        label: (r.label || "").trim(),
        prefix: r.prefix ?? "",
        suffix: r.suffix ?? "",
      }))
      // Drop fully-empty rules.
      .filter((r) => r.label || r.prefix || r.suffix),
    // Savings
    show_savings_estimate: els.setShowSavings.checked,
    typing_speed_wpm: clampInt(parseInt(els.setWpm.value, 10), 10, 200, 40),
    hourly_wage: Math.max(0, parseFloat(els.setWage.value) || 0),
    wage_currency: (els.setCurrency.value || "$").slice(0, 3),
    // About (retention)
    backup_retention_days: clampInt(parseInt(els.setBackupDays.value, 10), 1, 365, 14),
    log_retention_days: clampInt(parseInt(els.setLogDays.value, 10), 1, 365, 7),
    // Preserved - backend requires the full Settings struct.
    onboarding_completed: state.settings?.onboarding_completed ?? false,
  };
}

async function saveSettings() {
  const updated = collectSettingsForSave();
  try {
    state.settings = await invoke("update_settings", { newSettings: updated });
    applyTheme(state.settings.theme);
    applyAccentColor(state.settings.accent_color);
    applyCompact(state.settings.compact);
    // Mirror the saved Savings-tab values into the server's
    // per-user override so the admin dashboard's hours-and-money
    // estimate reflects this user's actual numbers. Silent no-op
    // when not Teams or not signed in.
    await syncProfileToServer(state.settings);
    setStatus("Settings saved", "ok");
    closeSettings();
    await refresh();
  } catch (err) {
    setStatus(`Error: ${err}`, "err");
  }
}

function closeSettings() {
  // Cancel revert - accent preview shouldn't linger on the launcher.
  applyAccentColor(state.settings?.accent_color || "");
  els.settings.classList.add("hidden");
  focusSearch();
}

// ---------- Reveal folders (backups / logs) ----------
async function openBackupsFolder() {
  try {
    await invoke("open_backups_folder");
  } catch (err) {
    setStatus(`Error: ${err}`, "err");
  }
}

async function openLogsFolder() {
  try {
    await invoke("open_logs_folder");
  } catch (err) {
    setStatus(`Error: ${err}`, "err");
  }
}

// ---------- Import / Export ----------
async function withDialogSuppressed(fn) {
  try {
    await invoke("suspend_hide_on_blur");
  } catch (_) {}
  try {
    return await fn();
  } finally {
    try {
      await invoke("resume_hide_on_blur");
    } catch (_) {}
  }
}

async function exportSnippets() {
  try {
    const path = await withDialogSuppressed(() =>
      saveDialog({
        defaultPath: "snippets.json",
        filters: [
          { name: "JSON", extensions: ["json"] },
          { name: "CSV", extensions: ["csv"] },
        ],
      })
    );
    if (!path) return;
    const format = path.toLowerCase().endsWith(".csv") ? "csv" : "json";
    const n = await invoke("export_snippets", { args: { path, format } });
    setStatus(`Exported ${n} snippet${n === 1 ? "" : "s"}`, "ok");
  } catch (err) {
    setStatus(`Error: ${err}`, "err");
  }
}

async function importSnippets() {
  try {
    const picked = await withDialogSuppressed(() =>
      openDialog({
        multiple: false,
        filters: [
          { name: "Snippet files (JSON, CSV)", extensions: ["json", "csv"] },
          { name: "JSON", extensions: ["json"] },
          { name: "CSV", extensions: ["csv"] },
        ],
      })
    );
    const path = asPath(picked);
    if (!path) return;
    const lower = path.toLowerCase();
    let format;
    if (lower.endsWith(".csv")) format = "csv";
    else format = "json";

    const result = await invoke("import_snippets", { args: { path, format } });
    const imported = result?.imported ?? 0;
    const skipped = result?.skipped_duplicates ?? 0;
    const parts = [`Imported ${imported} snippet${imported === 1 ? "" : "s"}`];
    if (skipped > 0) {
      parts.push(`skipped ${skipped} duplicate${skipped === 1 ? "" : "s"}`);
    }
    setStatus(parts.join(" - "), skipped > 0 && imported === 0 ? "err" : "ok");
    await refresh();
  } catch (err) {
    const message = typeof err === "string" ? err : err?.message || String(err);
    setStatus(`Import failed`, "err");
    alert(`Import failed:\n\n${message}`);
  }
}

// ---------- Events ----------
function bindEvents() {
  let searchTimer = null;
  els.search.addEventListener("input", () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      state.selectedIndex = 0;
      // reconcileSelectionAfterRefresh() re-seeds selectedIds from the new primary.
      state.selectedIds = new Set();
      state.anchorIndex = null;
      refresh();
    }, 80);
  });

  els.btnNew.addEventListener("click", () => openEditor());
  els.btnSettings.addEventListener("click", () => openSettings());
  els.btnNewFolder.addEventListener("click", () => createNewFolderPrompt());

  // Empty-space context menus. Item handlers stopPropagation, so these only
  // fire on blank areas - replaces the OS "reload / inspect" menu with a
  // section-appropriate creation shortcut.
  els.folderSidebar.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    showContextMenu(e.clientX, e.clientY, [
      { label: "New folder...", action: () => createNewFolderPrompt() },
    ]);
  });
  els.list.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    // Team library is read-only, so don't offer a create action there.
    if (state.selectedFolder === TEAM_FOLDER) {
      showContextMenu(e.clientX, e.clientY, [
        {
          label: "Team library is read-only",
          disabled: true,
        },
      ]);
      return;
    }
    showContextMenu(e.clientX, e.clientY, [
      { label: "New snippet...", action: () => openEditor() },
    ]);
  });

  els.editorSave.addEventListener("click", saveEditor);
  els.editorCancel.addEventListener("click", closeEditor);

  els.dupOpenExisting.addEventListener("click", openExistingConflict);
  els.dupEditTitle.addEventListener("click", () => {
    closeDuplicateWarning();
    els.editorTitleInput.focus();
    els.editorTitleInput.select();
  });
  els.dupSaveAnyway.addEventListener("click", async () => {
    const pending = state.pendingDuplicateSave;
    closeDuplicateWarning();
    if (pending) await doSaveEditor(pending);
  });

  els.varSubmit.addEventListener("click", submitVarPrompt);
  els.varCancel.addEventListener("click", closeVarPrompt);

  els.linkInsert.addEventListener("click", submitLinkPrompt);
  els.linkCancel.addEventListener("click", closeLinkPrompt);

  els.settings.querySelectorAll(".tab").forEach((tab) => {
    tab.addEventListener("click", () => activateTab(tab.dataset.tab));
  });

  // Accent color: picker, text field, reset all kept in sync. Unparseable text
  // flags invalid but doesn't touch the live preview (user may be mid-typing).
  els.setAccentColor.addEventListener("input", () => {
    const hex = els.setAccentColor.value; // "#rrggbb"
    els.setAccentText.value = hex;
    els.setAccentText.classList.remove("invalid");
    applyAccentColor(hex);
    updateAccentPreview(hex);
  });
  els.setAccentText.addEventListener("input", () => {
    const raw = els.setAccentText.value;
    const hex = normalizeAccent(raw);
    if (raw.trim() === "") {
      els.setAccentText.classList.remove("invalid");
      applyAccentColor("");
      updateAccentPreview("");
      return;
    }
    if (hex) {
      els.setAccentText.classList.remove("invalid");
      els.setAccentColor.value = hex;
      applyAccentColor(hex);
      updateAccentPreview(hex);
    } else {
      els.setAccentText.classList.add("invalid");
    }
  });
  els.btnAccentReset.addEventListener("click", () => {
    els.setAccentText.value = "";
    els.setAccentText.classList.remove("invalid");
    applyAccentColor("");
    // Seed swatch from theme accent so the next click doesn't jump.
    els.setAccentColor.value = readComputedAccentHex();
    updateAccentPreview("");
  });

  els.btnAddRule.addEventListener("click", () => {
    state.editingRules.push({ label: "New", prefix: "", suffix: "" });
    renderRuleEditor();
  });
  els.btnResetRules.addEventListener("click", () => {
    state.editingRules = deepCloneRules(DEFAULT_FORMAT_RULES);
    renderRuleEditor();
  });

  if (TEAMS_BUILD) {
    els.btnServerLogin.addEventListener("click", doServerLogin);
    els.btnServerSignup.addEventListener("click", doServerSignup);
    els.btnServerLogout.addEventListener("click", doServerLogout);
    els.btnServerSync.addEventListener("click", doServerSyncNow);
    if (els.btnServerOidc)
      els.btnServerOidc.addEventListener("click", () => doServerOidcStart());
    if (els.btnServerPasteToken)
      els.btnServerPasteToken.addEventListener("click", doServerPasteToken);
    if (els.trashClose) els.trashClose.addEventListener("click", closeTrashModal);
    if (els.trashModal) {
      // Click outside the card closes the modal, same UX as other
      // modals.
      els.trashModal.addEventListener("click", (e) => {
        if (e.target === els.trashModal) closeTrashModal();
      });
    }
  }

  els.btnOpenBackups.addEventListener("click", openBackupsFolder);
  els.btnOpenLogs.addEventListener("click", openLogsFolder);

  els.btnCheckUpdates.addEventListener("click", () => checkForUpdates({ silent: false }));
  els.updateInstall.addEventListener("click", installPendingUpdate);
  els.updateLater.addEventListener("click", dismissUpdateToast);

  els.setSave.addEventListener("click", saveSettings);

  // ---- Onboarding wiring ----
  // Each panel's primary actions get a click handler. Next/Skip
  // buttons are addressed by data attribute so adding a panel
  // doesn't require touching this block; only step-specific
  // buttons (typing test, hide-window, etc.) get explicit lookups.
  document.querySelectorAll("#onboarding [data-onboarding-next]").forEach((b) => {
    b.addEventListener("click", () => onboarding.advance());
  });
  document.querySelectorAll("#onboarding [data-onboarding-skip]").forEach((b) => {
    b.addEventListener("click", () => onboarding.skip());
  });
  document.querySelectorAll("#onboarding [data-onboarding-back]").forEach((b) => {
    b.addEventListener("click", () => onboarding.back());
  });
  const onbSigninOidc = document.getElementById("onboarding-signin-oidc");
  if (onbSigninOidc) onbSigninOidc.addEventListener("click", () => onboarding.startOidc());
  const onbPasteSubmit = document.getElementById("onboarding-paste-token-submit");
  if (onbPasteSubmit) onbPasteSubmit.addEventListener("click", () => onboarding.usePastedToken());
  const onbPasteInput = document.getElementById("onboarding-paste-token");
  if (onbPasteInput) {
    onbPasteInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        onboarding.usePastedToken();
      }
    });
  }
  const onbHotkeyHide = document.getElementById("onboarding-hotkey-hide");
  if (onbHotkeyHide) onbHotkeyHide.addEventListener("click", () => onboarding.hideAndTryHotkey());
  const onbTypingInput = document.getElementById("onboarding-typing-input");
  if (onbTypingInput) onbTypingInput.addEventListener("input", () => onboarding.onTypingInput());
  const onbTypingUse = document.getElementById("onboarding-typing-use");
  if (onbTypingUse) onbTypingUse.addEventListener("click", () => onboarding.useTypingResult());
  const onbTypingRestart = document.getElementById("onboarding-typing-restart");
  if (onbTypingRestart) onbTypingRestart.addEventListener("click", () => onboarding.primeTypingPanel());
  const onbWageSave = document.getElementById("onboarding-wage-save");
  if (onbWageSave) onbWageSave.addEventListener("click", () => onboarding.saveWage());
  const onbFinish = document.getElementById("onboarding-finish");
  if (onbFinish) onbFinish.addEventListener("click", () => onboarding.complete());
  if (els.btnReplayOnboarding) {
    els.btnReplayOnboarding.addEventListener("click", () => onboarding.replay());
  }

  // Hotkey fields: click-and-press to capture, instead of typing chord
  // strings by hand. Quick-add allows clearing via Backspace/Delete
  // (blank = disabled); the main launcher hotkey is always required.
  enableHotkeyCapture(els.setHotkey);
  enableHotkeyCapture(els.setQuickAddHotkey, { allowClear: true });
  els.setCancel.addEventListener("click", closeSettings);
  els.btnExport.addEventListener("click", exportSnippets);
  els.btnImport.addEventListener("click", importSnippets);

  // Dismiss context menu on outside click / blur / resize.
  document.addEventListener("mousedown", (e) => {
    if (!els.contextMenu.contains(e.target)) hideContextMenu();
  });
  window.addEventListener("blur", hideContextMenu);
  window.addEventListener("resize", hideContextMenu);

  document.addEventListener("keydown", onKeyDown);
}

function anyModalOpen() {
  // .app-modal covers the dynamic dialogs (text input, confirm, folder
  // picker) that are added to document.body at runtime - they wouldn't show
  // up in the static checks below, so a folder-create Enter would otherwise
  // fall through to the global paste handler.
  return (
    !!document.querySelector(".app-modal") ||
    !els.editor.classList.contains("hidden") ||
    !els.varPrompt.classList.contains("hidden") ||
    !els.linkPrompt.classList.contains("hidden") ||
    !els.settings.classList.contains("hidden") ||
    !els.dupWarn.classList.contains("hidden")
  );
}

async function onKeyDown(ev) {
  // Modal-specific handling first.
  if (!els.dupWarn.classList.contains("hidden")) {
    if (ev.key === "Escape") {
      ev.preventDefault();
      closeDuplicateWarning();
    }
    return;
  }
  if (!els.varPrompt.classList.contains("hidden")) {
    if (ev.key === "Escape") {
      ev.preventDefault();
      closeVarPrompt();
    } else if (ev.key === "Enter" && !ev.shiftKey) {
      ev.preventDefault();
      await submitVarPrompt();
    }
    return;
  }
  // Link prompt sits on top of the editor - handlers must run before editor's.
  if (!els.linkPrompt.classList.contains("hidden")) {
    if (ev.key === "Escape") {
      ev.preventDefault();
      closeLinkPrompt();
    } else if (ev.key === "Enter" && !ev.shiftKey) {
      ev.preventDefault();
      submitLinkPrompt();
    }
    return;
  }
  if (!els.editor.classList.contains("hidden")) {
    if (ev.key === "Escape") {
      ev.preventDefault();
      closeEditor();
    } else if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "s") {
      ev.preventDefault();
      await saveEditor();
    }
    return;
  }
  if (!els.settings.classList.contains("hidden")) {
    if (ev.key === "Escape") {
      ev.preventDefault();
      closeSettings();
    }
    return;
  }

  if (ev.key === "Escape" && !els.contextMenu.classList.contains("hidden")) {
    ev.preventDefault();
    hideContextMenu();
    return;
  }

  // Launcher view.
  if (ev.key === "Escape") {
    ev.preventDefault();
    if (els.search.value || state.activeTag || state.selectedFolder !== ALL_FOLDERS) {
      els.search.value = "";
      state.activeTag = null;
      state.selectedFolder = ALL_FOLDERS;
      state.selectedIndex = 0;
      await refresh();
    } else {
      await invoke("hide_window");
    }
    return;
  }

  if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "n") {
    ev.preventDefault();
    openEditor();
    return;
  }
  if ((ev.ctrlKey || ev.metaKey) && ev.key === ",") {
    ev.preventDefault();
    openSettings();
    return;
  }
  if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "e") {
    ev.preventDefault();
    const s = state.snippets[state.selectedIndex];
    if (s) openEditor(s);
    return;
  }
  if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "d") {
    ev.preventDefault();
    const s = state.snippets[state.selectedIndex];
    if (s) await duplicateSnippet(s.id);
    return;
  }
  if (
    ev.key === "Delete" &&
    document.activeElement !== els.search &&
    !anyModalOpen()
  ) {
    ev.preventDefault();
    await deleteCurrent();
    return;
  }
  if (ev.key === "ArrowDown") {
    ev.preventDefault();
    if (state.selectedIndex < state.snippets.length - 1) {
      // Plain arrow collapses to single; Shift+Arrow extends. Explorer/Finder semantics.
      if (ev.shiftKey) {
        extendSelectionTo(state.selectedIndex + 1);
      } else {
        selectOnly(state.selectedIndex + 1);
      }
    }
    return;
  }
  if (ev.key === "ArrowUp") {
    ev.preventDefault();
    if (state.selectedIndex > 0) {
      if (ev.shiftKey) {
        extendSelectionTo(state.selectedIndex - 1);
      } else {
        selectOnly(state.selectedIndex - 1);
      }
    }
    return;
  }
  if (ev.key === "Enter" && !anyModalOpen()) {
    ev.preventDefault();
    await usePastedSnippet(ev.shiftKey);
    return;
  }
}

// ---------- Status ----------
let statusTimer = null;
function setStatus(msg, kind = "") {
  // If a modal is open, route the status into it instead of the main
  // footer bar - the modal covers the footer at default window size,
  // which means messages like "Uploaded 3 snippets to the server"
  // would otherwise be invisible to the user. We pick the deepest
  // open modal (latest opened tends to be DOM-ordered later) so a
  // nested confirm/dup-warn over the settings modal still surfaces
  // its status in the right place.
  const openModals = document.querySelectorAll(".modal:not(.hidden)");
  const targetModal = openModals.length > 0 ? openModals[openModals.length - 1] : null;
  if (targetModal) {
    const card = targetModal.querySelector(".modal-card");
    if (card) {
      let slot = card.querySelector(":scope > .modal-status");
      if (!slot) {
        slot = document.createElement("div");
        slot.className = "modal-status";
        card.appendChild(slot);
      }
      slot.textContent = msg;
      slot.className = `modal-status ${kind}`;
      clearTimeout(statusTimer);
      statusTimer = setTimeout(() => {
        slot.textContent = "";
        slot.className = "modal-status";
      }, 3500);
      return;
    }
  }
  els.status.textContent = msg;
  els.status.className = `status ${kind}`;
  clearTimeout(statusTimer);
  statusTimer = setTimeout(() => {
    els.status.textContent = "";
    els.status.className = "status";
  }, 3500);
}

// ---------- Savings estimate ----------
function computeSavings(snippets, wpm, hourlyWage) {
  let totalChars = 0;
  for (const s of snippets) {
    const uses = Number(s.usage_count) || 0;
    if (uses <= 0) continue;
    totalChars += (s.body ? s.body.length : 0) * uses;
  }
  const safeWpm = Math.max(1, wpm || 40);
  const words = totalChars / 5;
  const minutes = words / safeWpm;
  const seconds = minutes * 60;
  const hours = minutes / 60;
  const money = hourlyWage > 0 ? hours * hourlyWage : 0;
  return { totalChars, seconds, hours, money };
}

function formatDuration(totalSeconds) {
  if (totalSeconds < 60) {
    return `${Math.round(totalSeconds)}s`;
  }
  const totalMinutes = Math.round(totalSeconds / 60);
  const h = Math.floor(totalMinutes / 60);
  const m = totalMinutes % 60;
  if (h === 0) return `${m}m`;
  if (h < 24) return m === 0 ? `${h}h` : `${h}h ${m}m`;
  const days = Math.floor(h / 24);
  const rh = h % 24;
  return rh === 0 ? `${days}d` : `${days}d ${rh}h`;
}

function formatMoney(amount, currency) {
  const sym = currency || "$";
  if (amount >= 100) return `${sym}${Math.round(amount).toLocaleString()}`;
  return `${sym}${amount.toFixed(2)}`;
}

function renderSavings() {
  const s = state.settings;
  if (!s || !s.show_savings_estimate) {
    els.savings.classList.add("hidden");
    els.savings.textContent = "";
    return;
  }
  const { seconds, money } = computeSavings(
    state.allSnippets || [],
    s.typing_speed_wpm || 40,
    s.hourly_wage || 0,
  );
  if (seconds < 1) {
    els.savings.classList.add("hidden");
    return;
  }
  const timeText = formatDuration(seconds);
  const moneyText = money > 0 ? ` | ${formatMoney(money, s.wage_currency)}` : "";
  els.savings.textContent = `Saved: ${timeText}${moneyText}`;
  els.savings.classList.remove("hidden");
}

function clampInt(n, min, max, fallback) {
  if (!Number.isFinite(n)) return fallback;
  return Math.max(min, Math.min(max, Math.round(n)));
}
