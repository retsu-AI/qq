import { test } from "node:test";
import assert from "node:assert/strict";
import { Drafts } from "../src/lib/drafts.ts";

test("rejection keeps the exact draft for retry", async () => {
  const drafts = new Drafts();
  drafts.set("a", "  original message  ");
  await assert.rejects(
    drafts.submit("a", async () => {
      throw new Error("offline");
    }),
    /offline/,
  );
  assert.equal(drafts.get("a"), "  original message  ");
  await drafts.submit("a", async (text) =>
    assert.equal(text, "original message"),
  );
  assert.equal(drafts.get("a"), "");
});

test("acknowledgement cannot clear another session or a newer draft", async () => {
  const drafts = new Drafts();
  drafts.set("a", "first");
  drafts.set("b", "other session");
  let acknowledge;
  const pending = drafts.submit(
    "a",
    () =>
      new Promise((resolve) => {
        acknowledge = resolve;
      }),
  );
  await assert.rejects(
    drafts.submit("a", async () => {}),
    /awaiting acknowledgement/,
  );
  drafts.set("a", "newer text");
  acknowledge();
  await pending;
  assert.equal(drafts.get("a"), "newer text");
  assert.equal(drafts.get("b"), "other session");
});

test("a remounted composer observes pending state and the eventual acknowledgement", async () => {
  const drafts = new Drafts();
  drafts.set("a", "pending message");
  let acknowledge;
  const result = drafts.submit(
    "a",
    () =>
      new Promise((resolve) => {
        acknowledge = resolve;
      }),
  );
  assert.equal(drafts.isPending("a"), true);
  const observed = [];
  const unsubscribe = drafts.subscribe(() =>
    observed.push([drafts.get("a"), drafts.isPending("a")]),
  );
  acknowledge();
  await result;
  assert.deepEqual(observed.at(-1), ["", false]);
  unsubscribe();
});
