import { test } from "node:test";
import assert from "node:assert/strict";

// In-memory chrome.storage.local, installed before importing storage.js so
// the module's accessors have something to talk to.
const store = {};
globalThis.chrome = {
  storage: {
    local: {
      get: (keys, cb) => {
        const out = {};
        for (const k of Array.isArray(keys) ? keys : [keys]) {
          if (k in store) out[k] = store[k];
        }
        cb(out);
      },
      set: (obj, cb) => {
        Object.assign(store, obj);
        cb && cb();
      },
      remove: (keys, cb) => {
        for (const k of Array.isArray(keys) ? keys : [keys]) delete store[k];
        cb && cb();
      },
    },
    managed: { get: (_keys, cb) => cb({}) },
  },
};

const storage = await import("../src/shared/storage.js");

test("clearSession keeps personal snippets, drops auth + team library", async () => {
  store.token = "jwt";
  store.user = { id: "u1" };
  store.cache_personal = { items: { a: { id: "a", title: "mine" } }, hwm: 5 };
  store.cache_library = { items: { b: { id: "b" } }, hwm: 3 };

  await storage.clearSession();

  // Signed out: token, identity, and the sign-in-only team library are gone.
  assert.equal(store.token, undefined);
  assert.equal(store.user, undefined);
  assert.equal(store.cache_library, undefined);
  // Personal snippets (and their sync high-water mark) survive so the
  // extension stays usable offline / signed out.
  assert.deepEqual(store.cache_personal, { items: { a: { id: "a", title: "mine" } }, hwm: 5 });
});

test("ticket-link config defaults to disabled and round-trips", async () => {
  delete store.ticket_link;
  // Unset -> safe default: feature off, no pattern.
  assert.deepEqual(await storage.getTicketLink(), { enabled: false, url_pattern: "" });

  await storage.setTicketLink({ enabled: true, url_pattern: "id=(\\d+)" });
  assert.deepEqual(await storage.getTicketLink(), { enabled: true, url_pattern: "id=(\\d+)" });
});

test("clearSession drops server-provided ticket-link config", async () => {
  store.token = "jwt";
  store.ticket_link = { enabled: true, url_pattern: "id=(\\d+)" };

  await storage.clearSession();

  // Server config must not outlive the session (or a server switch).
  assert.equal(store.ticket_link, undefined);
  assert.deepEqual(await storage.getTicketLink(), { enabled: false, url_pattern: "" });
});
