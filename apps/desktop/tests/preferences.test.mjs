import { test } from "node:test";
import assert from "node:assert/strict";
import {
  fontName,
  fontStack,
  parsePreferences,
} from "../src/lib/preferences.ts";
test("preference migration defaults and invalid values remain usable", () => {
  const defaults = {
    theme: "system",
    fontSize: 14,
    notifications: false,
    sidePanel: null,
    interfaceFont: null,
    codeFont: null,
  };
  for (const source of [
    null,
    "broken",
    "null",
    '{"theme":"unknown","fontSize":500,"notifications":"yes"}',
  ]) {
    assert.deepEqual(parsePreferences(source), defaults);
  }
  const saved = {
    theme: "dark",
    fontSize: 18,
    notifications: true,
    sidePanel: "details",
    interfaceFont: "Inter",
    codeFont: "JetBrains Mono",
  };
  assert.deepEqual(parsePreferences(JSON.stringify(saved)), saved);
  // An open Changes panel from before the details panel stays open.
  assert.equal(parsePreferences('{"changesPanel":true}').sidePanel, "changes");
  assert.equal(parsePreferences('{"sidePanel":"other"}').sidePanel, null);
});

test("font names that could escape a CSS font stack are refused", () => {
  assert.equal(fontName("  Fira   Code "), "Fira Code");
  assert.equal(fontName("Noto Sans CJK JP"), "Noto Sans CJK JP");
  for (const bad of [
    'Inter", serif; color: red',
    "Inter;}",
    "url(x)",
    "",
    "x".repeat(65),
    42,
  ])
    assert.equal(fontName(bad), null);
  assert.deepEqual(
    parsePreferences('{"interfaceFont":"a\\"b","codeFont":"Consolas"}')
      .interfaceFont,
    null,
  );
  // A missing font falls back to the default family.
  assert.match(fontStack("Inter", false), /^"Inter", "Segoe UI Variable Text"/);
  assert.match(fontStack(null, true), /^"Cascadia Code"/);
});
