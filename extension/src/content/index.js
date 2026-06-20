// Content script: hosts the in-page launcher overlay. Phase 1 proves
// the wiring that the rest builds on - the command reaches here, the
// overlay mounts in an isolated shadow root (so page CSS can't touch
// it), and the editable element that was focused BEFORE the overlay
// opened is captured as the insertion target.

let host = null;
let captured = null;

function captureTarget() {
  const el = document.activeElement;
  if (!el) return null;
  const tag = el.tagName?.toLowerCase();
  if (tag === "input" || tag === "textarea" || el.isContentEditable) {
    return el;
  }
  return null;
}

function closeOverlay() {
  if (host) {
    host.remove();
    host = null;
  }
}

function openOverlay() {
  captured = captureTarget();

  host = document.createElement("div");
  host.id = "snipdesk-overlay-host";
  host.style.cssText =
    "position:fixed;inset:0;z-index:2147483647;";
  const root = host.attachShadow({ mode: "open" });

  const targetLabel = captured
    ? `${captured.tagName.toLowerCase()}${captured.isContentEditable ? " (contenteditable)" : ""}`
    : "no editable field focused";

  root.innerHTML = `
    <style>
      .backdrop { position:fixed; inset:0; background:rgba(0,0,0,0.35); display:flex; align-items:flex-start; justify-content:center; }
      .panel { margin-top:12vh; width:min(640px,92vw); background:#1e1f22; color:#e6e6e6; border-radius:10px; box-shadow:0 12px 40px rgba(0,0,0,0.5); font:14px/1.4 -apple-system,Segoe UI,Roboto,sans-serif; overflow:hidden; }
      .head { padding:14px 16px; border-bottom:1px solid #333; font-weight:600; }
      .body { padding:16px; color:#b8b8b8; }
      kbd { background:#2b2d31; border:1px solid #444; border-radius:4px; padding:1px 6px; font-size:12px; }
    </style>
    <div class="backdrop">
      <div class="panel">
        <div class="head">SnipDesk</div>
        <div class="body">
          Launcher scaffold is live. Insertion target: <strong>${targetLabel}</strong>.<br>
          Press <kbd>Esc</kbd> to close.
        </div>
      </div>
    </div>
  `;

  root.querySelector(".backdrop").addEventListener("mousedown", (e) => {
    if (e.target.classList.contains("backdrop")) closeOverlay();
  });

  document.documentElement.appendChild(host);
}

function toggleOverlay() {
  if (host) closeOverlay();
  else openOverlay();
}

document.addEventListener(
  "keydown",
  (e) => {
    if (e.key === "Escape" && host) {
      e.preventDefault();
      closeOverlay();
    }
  },
  true,
);

chrome.runtime.onMessage.addListener((msg) => {
  if (msg?.type === "toggle-launcher") toggleOverlay();
});
