const statusEl = document.getElementById("status");

chrome.runtime.sendMessage({ type: "ping" }, (res) => {
  if (chrome.runtime.lastError || !res?.ok) {
    statusEl.textContent = "Background worker not responding.";
    return;
  }
  statusEl.innerHTML = '<span class="ok">Ready.</span> Press the launcher shortcut on any page.';
});

document.getElementById("open-manager").addEventListener("click", () => {
  chrome.runtime.openOptionsPage();
});
