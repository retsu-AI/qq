import { test } from "node:test";
import assert from "node:assert/strict";
import { parsePreferences } from "../src/lib/preferences.ts";
test("preference migration defaults and invalid values remain usable", () => {
  const defaults = { theme: "system", fontSize: 14, notifications: false };
  for (const source of [
    null,
    "broken",
    "null",
    '{"theme":"unknown","fontSize":500,"notifications":"yes"}',
  ]) {
    assert.deepEqual(parsePreferences(source), defaults);
  }
  const saved = { theme: "dark", fontSize: 18, notifications: true };
  assert.deepEqual(parsePreferences(JSON.stringify(saved)), saved);
});
