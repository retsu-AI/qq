import { test } from "node:test";
import assert from "node:assert/strict";
import {
  findSlash,
  insertSlash,
  parseSideCommand,
  slashSuggestions,
} from "../src/lib/slash.ts";

test("a slash command is only completed at the start, before a space", () => {
  assert.deepEqual(findSlash("/", 1), { start: 0, query: "" });
  assert.deepEqual(findSlash("/shi", 4), { start: 0, query: "shi" });
  assert.equal(findSlash("/ship now", 9), null);
  assert.equal(findSlash("see /ship", 9), null);
  assert.equal(findSlash("text", 4), null);
});

const skills = [
  { name: "ship", kind: "command", description: "Cut a release" },
  { name: "review", kind: "skill", description: "Review guidance" },
  { name: "preview-docs", kind: "command", description: "" },
];

test("suggestions rank prefix matches first and include side commands", () => {
  const side = { available: true, running: false };
  assert.deepEqual(
    slashSuggestions("rev", skills, side).map((s) => s.value),
    ["review", "preview-docs"],
  );
  assert.deepEqual(
    slashSuggestions("b", skills, side).map((s) => s.value),
    ["btw", "btw-new"],
  );
  assert.deepEqual(
    slashSuggestions("btw", skills, { available: true, running: true }).map(
      (s) => s.value,
    ),
    ["btw", "btw-new", "btw-cancel"],
  );
  assert.deepEqual(
    slashSuggestions("", skills, { available: false, running: false }).map(
      (s) => s.kind,
    ),
    ["command", "skill", "command"],
  );
});

test("goal commands are offered only where goals are available", () => {
  const side = { available: false, running: false };
  assert.deepEqual(slashSuggestions("go", skills, side).map((s) => s.value), []);
  assert.deepEqual(
    slashSuggestions("goal", skills, side, true).map((s) => s.value),
    ["goal", "goal edit", "goal pause", "goal resume"],
  );
});

test("choosing a command replaces the typed name and keeps the rest", () => {
  assert.deepEqual(insertSlash("/sh", 3, "ship"), { text: "/ship ", caret: 6 });
  assert.deepEqual(insertSlash("/sh the release", 3, "ship"), {
    text: "/ship the release",
    caret: 6,
  });
});

test("side commands parse; other slashes are ordinary prompts", () => {
  assert.deepEqual(parseSideCommand("/btw why is CI red?"), {
    kind: "ask",
    question: "why is CI red?",
    newThread: false,
  });
  assert.deepEqual(parseSideCommand("/ask  multi\nline "), {
    kind: "ask",
    question: "multi\nline",
    newThread: false,
  });
  assert.deepEqual(parseSideCommand("/btw-new fresh"), {
    kind: "ask",
    question: "fresh",
    newThread: true,
  });
  assert.deepEqual(parseSideCommand("/btw"), { kind: "show" });
  assert.deepEqual(parseSideCommand("/btw-cancel"), { kind: "cancel" });
  assert.equal(parseSideCommand("/btwx hi"), null);
  assert.equal(parseSideCommand("/ship"), null);
  assert.equal(parseSideCommand("please /btw this"), null);
});
