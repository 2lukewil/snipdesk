import { test } from "node:test";
import assert from "node:assert/strict";
import { extractVarNames, substitute, splitForPreview } from "../src/shared/variables.js";

test("extractVarNames returns unique names in order", () => {
  assert.deepEqual(extractVarNames("Hi {name}, your {item} and {name}"), ["name", "item"]);
});

test("extractVarNames ignores braces with non-name content", () => {
  assert.deepEqual(extractVarNames("plain text, {bad spaces}, {also bad}"), []);
});

test("extractVarNames handles repeated calls (lastIndex reset)", () => {
  const body = "{a} {b}";
  assert.deepEqual(extractVarNames(body), ["a", "b"]);
  assert.deepEqual(extractVarNames(body), ["a", "b"]);
});

test("substitute replaces known vars and leaves unknown intact", () => {
  assert.equal(
    substitute("Dear {name}, ref {ticket}", { name: "Sam" }),
    "Dear Sam, ref {ticket}",
  );
});

test("substitute handles hyphen/underscore/digit names", () => {
  assert.equal(substitute("{first_name}-{id-2}", { first_name: "Jo", "id-2": "9" }), "Jo-9");
});

test("splitForPreview yields text and var chunks", () => {
  assert.deepEqual(splitForPreview("a {x} b"), [
    { type: "text", text: "a " },
    { type: "var", name: "x" },
    { type: "text", text: " b" },
  ]);
});

test("splitForPreview with no vars is a single text chunk", () => {
  assert.deepEqual(splitForPreview("plain"), [{ type: "text", text: "plain" }]);
});
