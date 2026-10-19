import { test } from "node:test";
import assert from "node:assert/strict";
import { dropMentions, insertMentions } from "../src/lib/drop.ts";

test("dropped files inside the project become relative mentions", () => {
  const result = dropMentions("C:\\work\\qq", [
    "C:\\work\\qq\\src\\main.rs",
    "c:\\WORK\\qq\\docs",
    "\\\\?\\C:\\work\\qq\\Cargo.toml",
    "C:\\work\\qq\\src\\main.rs",
  ]);
  assert.deepEqual(result.mentions, ["src/main.rs", "docs", "Cargo.toml"]);
  assert.deepEqual(result.skipped, []);
});

test("files outside the project, the root itself and spaced paths are skipped", () => {
  const result = dropMentions("C:\\work\\qq", [
    "C:\\work\\qq-other\\a.rs",
    "D:\\elsewhere\\b.rs",
    "C:\\work\\qq",
    "C:\\work\\qq\\my notes.md",
  ]);
  assert.deepEqual(result.mentions, []);
  assert.deepEqual(
    result.skipped.map((s) => s.reason),
    ["outside", "outside", "outside", "spaces"],
  );
});

test("mentions insert at the caret with separating spaces", () => {
  assert.deepEqual(insertMentions("", 0, ["a.rs"]), { text: "@a.rs ", caret: 6 });
  assert.deepEqual(insertMentions("fix", 3, ["a.rs", "b.rs"]), {
    text: "fix @a.rs @b.rs ",
    caret: 16,
  });
  assert.deepEqual(insertMentions("see  now", 4, ["a.rs"]), {
    text: "see @a.rs now",
    caret: 9,
  });
});
