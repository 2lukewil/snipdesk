import { defineManifest } from "@crxjs/vite-plugin";
import pkg from "./package.json" with { type: "json" };

// host_permissions and content_scripts both span all sites: agents
// paste into whatever web tool they use (WHMCS, mail, chat), and the
// configured server URL isn't known until runtime, so the background
// worker needs cross-origin fetch to wherever it's pointed. For an
// internally force-installed extension this breadth is expected.
export default defineManifest({
  manifest_version: 3,
  name: "SnipDesk",
  version: pkg.version,
  // Pins the extension ID (pmbbmppiinigigajakmffkchlibmdebo) so the
  // launchWebAuthFlow redirect URL is stable and the server can
  // allowlist it. Public key only; the matching private key is never
  // committed. A Web Store listing reassigns its own key/ID, at which
  // point the server's allowed redirect is updated to match.
  key: "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAvVhd+szMi+GZo/XL2WxlMbse2kq5v3z48lFAdwfhRyRryCKu5rqQ3Cfwitr6+kUTv5FmPx994Hr5QoOPPljRkfZ4cxPJkblncD/d511+SmQwb4xaH0qfxk9HutgSBl0DMoQXRYUTxD0KXeToUdHy1lVSOHucFUBIqSRA4F0oGefm63MmI00H+4Fot7klOxjiEH+cwfslCVZDaK9yEJBoAgrlJ7jpLwnAH9MnM2dM9OoDTIjtm3hOfCBO4Z7Ytx4lQh3DjbzYdf+HygyKoru0TZ6jhMjvpEgIWhFEb9QMvGoguloIjMl+ZHLI+62ebL/kMmNq0vyTdHBFRGQ8EMwe5wIDAQAB",
  description:
    "Snippet launcher and team library for the browser. Search, fill variables, insert.",
  icons: {
    16: "icons/icon16.png",
    32: "icons/icon32.png",
    48: "icons/icon48.png",
    128: "icons/icon128.png",
  },
  action: {
    default_popup: "src/popup/index.html",
    default_title: "SnipDesk",
    default_icon: {
      16: "icons/icon16.png",
      32: "icons/icon32.png",
    },
  },
  options_page: "src/manager/index.html",
  background: {
    service_worker: "src/background/index.js",
    type: "module",
  },
  content_scripts: [
    {
      matches: ["<all_urls>"],
      js: ["src/content/index.js"],
      run_at: "document_idle",
      all_frames: false,
    },
  ],
  commands: {
    "open-launcher": {
      suggested_key: { default: "Ctrl+Shift+Space", mac: "Command+Shift+Space" },
      description: "Open the SnipDesk launcher",
    },
  },
  permissions: ["storage", "unlimitedStorage", "identity", "scripting", "alarms", "contextMenus"],
  host_permissions: ["<all_urls>"],
});
