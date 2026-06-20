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
  permissions: ["storage", "unlimitedStorage", "identity", "scripting", "alarms"],
  host_permissions: ["<all_urls>"],
});
