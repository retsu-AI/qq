import { test } from "node:test";
import assert from "node:assert/strict";
import {
  findMention,
  insertMention,
  specialSuggestions,
} from "../src/lib/mentions.ts";

test("a mention is found at message start and after separators", () => {
  assert.deepEqual(findMention("@src", 4), { start: 0, query: "src" });
  assert.deepEqual(findMention("look at @src/ma", 15), {
    start: 8,
    query: "src/ma",
  });
  assert.deepEqual(findMention("(@a", 3), { start: 1, query: "a" });
  assert.deepEqual(findMention("see @", 5), { start: 4, query: "" });
});

test("emails, escaped @@, ended tokens and fenced code are not mentions", () => {
  assert.equal(findMention("mail me@host", 12), null);
  assert.equal(findMention("literal @@x", 11), null);
  assert.equal(findMention("@a.rs done", 10), null);
  assert.equal(findMention("```\n@src", 8), null);
  assert.deepEqual(findMention("```x```\n@src", 12), { start: 8, query: "src" });
});

test("only the text before the caret is considered", () => {
  const text = "@src/main.rs please";
  assert.deepEqual(findMention(text, 4), { start: 0, query: "src" });
  assert.equal(findMention(text, text.length), null);
});

test("files are finished with a space; directories keep completing", () => {
  const file = insertMention("fix @ma now", 7, { start: 4, query: "ma" }, "src/main.rs");
  assert.equal(file.text, "fix @src/main.rs now");
  assert.equal(file.caret, "fix @src/main.rs ".length);
  const end = insertMention("fix @ma", 7, { start: 4, query: "ma" }, "src/main.rs");
  assert.equal(end.text, "fix @src/main.rs ");
  assert.equal(end.caret, end.text.length);
  const dir = insertMention("@sr", 3, { start: 0, query: "sr" }, "src/");
  assert.equal(dir.text, "@src/");
  assert.equal(dir.caret, 5);
});

test("special references complete from their prefix", () => {
  assert.deepEqual(
    specialSuggestions("d").map((s) => s.value),
    ["diff"],
  );
  assert.deepEqual(
    specialSuggestions("").map((s) => s.value),
    ["diff", "web:", "sha:"],
  );
  assert.deepEqual(specialSuggestions("src/"), []);
  assert.deepEqual(specialSuggestions("diff"), []);
});
