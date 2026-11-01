import { test } from "node:test";
import assert from "node:assert/strict";
import { emojiValue, parseProjectIcons } from "../src/lib/project-icons.ts";

test("project icons keep valid entries and drop the rest", () => {
  const png = "data:image/png;base64,iVBORw0KGgo=";
  const icons = parseProjectIcons(
    JSON.stringify({
      "C:/a": { kind: "initials", color: "teal" },
      "C:/b": { kind: "symbol", color: "nope", symbol: "rocket" },
      "C:/c": { kind: "emoji", color: "red", emoji: "🚀" },
      "C:/d": { kind: "image", image: png },
      "C:/e": { kind: "symbol", color: "red", symbol: "skull" },
      "C:/f": { kind: "image", image: "https://example.com/x.png" },
      "C:/g": { kind: "image", image: "data:image/svg+xml;base64,PHN2Zz4=" },
      "C:/h": { kind: "emoji", color: "red", emoji: "QQ" },
    }),
  );
  assert.deepEqual({ ...icons }, {
    "C:/a": { kind: "initials", color: "teal" },
    "C:/b": { kind: "symbol", color: "blue", symbol: "rocket" },
    "C:/c": { kind: "emoji", color: "red", emoji: "🚀" },
    "C:/d": { kind: "image", image: png },
  });
  for (const broken of [null, "broken", "[]", "null", '"text"'])
    assert.deepEqual({ ...parseProjectIcons(broken) }, {});
});

test("an emoji icon is exactly one emoji", () => {
  assert.equal(emojiValue(" 👩‍💻 "), "👩‍💻");
  assert.equal(emojiValue("🇯🇵"), "🇯🇵");
  for (const bad of ["", "a", "🚀🚀", "🚀a", 7]) assert.equal(emojiValue(bad), null);
});
