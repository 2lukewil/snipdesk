// Service worker: owns auth + network in later phases. Phase 1 wires
// the launcher command through to the active tab's content script and
// answers a ping so the popup can confirm the worker is alive.

chrome.runtime.onInstalled.addListener(() => {
  console.info("[snipdesk] installed");
});

chrome.commands.onCommand.addListener((command) => {
  if (command !== "open-launcher") return;
  chrome.tabs.query({ active: true, currentWindow: true }, (tabs) => {
    const tab = tabs[0];
    if (tab?.id != null) {
      chrome.tabs.sendMessage(tab.id, { type: "toggle-launcher" }).catch(() => {});
    }
  });
});

chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg?.type === "ping") {
    sendResponse({ ok: true });
  }
  return false;
});
