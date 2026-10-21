import { test } from "node:test";
import assert from "node:assert/strict";
import { phoneMenus } from "../src/lib/menu.ts";

const item = (label, extra = {}) => ({ label, onSelect: () => undefined, ...extra });
const labels = (menus) =>
  menus.flatMap((menu) => menu.items.map((entry) => (entry === "separator" ? "—" : entry.label)));

test("phones get one menu without desktop-only items", () => {
  const menus = phoneMenus([
    { label: "File", mnemonic: "f", items: [item("New session"), item("Open project…", { desktopOnly: true })] },
    { label: "Edit", mnemonic: "e", items: [item("Undo", { desktopOnly: true }), "separator", item("Paste", { desktopOnly: true })] },
    { label: "View", mnemonic: "v", items: ["separator", item("Dark theme"), "separator", "separator", item("Light theme"), "separator"] },
    { label: "Help", mnemonic: "h", items: [item("About QQ")] },
  ]);
  assert.equal(menus.length, 1);
  assert.equal(menus[0].label, "Menu");
  // An emptied menu leaves no doubled, leading or trailing separator.
  assert.deepEqual(labels(menus), ["New session", "—", "Dark theme", "—", "Light theme", "—", "About QQ"]);
});

test("a menu bar with only desktop items leaves no phone menu", () => {
  assert.deepEqual(
    phoneMenus([{ label: "Edit", mnemonic: "e", items: [item("Undo", { desktopOnly: true }), "separator"] }]),
    [],
  );
});
