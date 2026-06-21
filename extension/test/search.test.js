import { test } from "node:test";
import assert from "node:assert/strict";
import { filterSnippets, sortSnippets } from "../src/shared/search.js";

const snips = [
  { title: "Billing reply", body: "thanks", tags: ["billing"], uses: 2 },
  { title: "Refund note", body: "billing refund", tags: ["money"], uses: 10 },
  { title: "Welcome", body: "hello there", tags: ["onboarding"], uses: 0 },
  { title: "billed late", body: "x", tags: [], uses: 5 },
];

test("empty query returns a copy of all snippets", () => {
  const out = filterSnippets(snips, "");
  assert.equal(out.length, snips.length);
  assert.notEqual(out, snips); // a copy, not the same array
});

test("ranks title-prefix above title-substring above tag above body", () => {
  const out = filterSnippets(snips, "billing");
  // "Billing reply" (title prefix) first, then a tag match, then body match.
  assert.equal(out[0].title, "Billing reply");
  const titles = out.map((s) => s.title);
  assert.ok(titles.includes("Refund note")); // body "billing refund"
  assert.ok(!titles.includes("Welcome")); // no match anywhere
});

test("query is case-insensitive and trimmed", () => {
  assert.equal(filterSnippets(snips, "  WELCOME ")[0].title, "Welcome");
});

test("no matches yields empty list", () => {
  assert.deepEqual(filterSnippets(snips, "zzzznope"), []);
});

test("sortSnippets by usage desc, then title", () => {
  const out = sortSnippets(snips, true).map((s) => s.title);
  assert.deepEqual(out, ["Refund note", "billed late", "Billing reply", "Welcome"]);
});

test("sortSnippets alphabetical when not by usage", () => {
  const out = sortSnippets(snips, false).map((s) => s.title);
  assert.deepEqual(out, ["billed late", "Billing reply", "Refund note", "Welcome"]);
});

test("sortSnippets does not mutate the input", () => {
  const copy = snips.slice();
  sortSnippets(snips, true);
  assert.deepEqual(snips, copy);
});
