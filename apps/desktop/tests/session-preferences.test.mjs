import { test } from "node:test";
import assert from "node:assert/strict";
import {
  parseSessionPreferences,
  matchesSession,
  updateSessionPreferences,
} from "../src/lib/session-preferences.ts";

test("aliases and archive survive reload and restore without changing history", () => {
  const empty = parseSessionPreferences(null);
  const saved = updateSessionPreferences(empty, "session", {
    title: "  Release fix  ",
    archived: true,
  });
  const loaded = parseSessionPreferences(JSON.stringify(saved));
  assert.equal(loaded.aliases.session, "Release fix");
  assert.deepEqual(loaded.archived, ["session"]);
  const restored = updateSessionPreferences(loaded, "session", {
    title: "",
    archived: false,
  });
  assert.equal(restored.aliases.session, undefined);
  assert.deepEqual(restored.archived, []);
  assert.deepEqual(empty.archived, []);
});
test("search matches all words regardless of case across title and recent output", () => {
  assert.equal(
    matchesSession("RELEASE tests", "Release fix", "Tests passed"),
    true,
  );
  assert.equal(
    matchesSession("release missing", "Release fix", "Tests passed"),
    false,
  );
  assert.equal(matchesSession("  ", "Anything", ""), true);
});
test("malformed metadata recovers and long names are refused", () => {
  assert.deepEqual(parseSessionPreferences("bad json"), {
    aliases: {},
    archived: [],
  });
  assert.throws(
    () =>
      updateSessionPreferences(parseSessionPreferences(null), "a", {
        title: "x".repeat(161),
      }),
    /160/,
  );
});
