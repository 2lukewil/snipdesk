import { test } from "node:test";
import assert from "node:assert/strict";
import { validateSnippet, LIMITS } from "../src/shared/validate.js";

const ok = { title: "Greeting", body: "Hello {name}", tags: ["mail"], folder_path: "Replies" };

const BELL = String.fromCharCode(0x07);
const NUL = String.fromCharCode(0x00);
const TAB = String.fromCharCode(0x09);
const LF = String.fromCharCode(0x0a);

test("a well-formed snippet passes", () => {
  assert.equal(validateSnippet(ok), null);
});

test("title is required", () => {
  assert.match(validateSnippet({ ...ok, title: "   " }), /Title is required/);
});

test("title length is capped", () => {
  assert.match(validateSnippet({ ...ok, title: "a".repeat(LIMITS.TITLE + 1) }), /Title is too long/);
});

test("control chars rejected in title", () => {
  assert.match(validateSnippet({ ...ok, title: "bad" + BELL + "bell" }), /control characters/);
});

test("newline and tab allowed in body, other control chars rejected", () => {
  assert.equal(validateSnippet({ ...ok, body: "line1" + LF + "line2" + TAB + "x" }), null);
  assert.match(validateSnippet({ ...ok, body: "nul" + NUL + "byte" }), /control characters/);
});

test("tag count and length are capped", () => {
  const many = Array.from({ length: LIMITS.MAX_TAGS + 1 }, (_, i) => `t${i}`);
  assert.match(validateSnippet({ ...ok, tags: many }), /Too many tags/);
  assert.match(validateSnippet({ ...ok, tags: ["x".repeat(LIMITS.TAG + 1)] }), /Tag .* too long/);
});

test("folder path length is capped and optional", () => {
  assert.equal(validateSnippet({ ...ok, folder_path: null }), null);
  assert.match(
    validateSnippet({ ...ok, folder_path: "p".repeat(LIMITS.FOLDER + 1) }),
    /Folder path is too long/,
  );
});
