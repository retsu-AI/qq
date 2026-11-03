import { test } from "node:test";
import assert from "node:assert/strict";
import { approvalShortcut } from "../src/lib/approval-shortcuts.ts";
test("an obscured, editable, repeated or non-selected approval cannot respond to a shortcut", () => {
  for (const key of ["Y", "N"])
    assert.equal(approvalShortcut(key, { blocked: true }), null);
  assert.deepEqual(approvalShortcut("Y", { blocked: false }), { type: "once" });
  assert.deepEqual(approvalShortcut("N", { blocked: false }), { type: "deny" });
});

test("approvals offer no standing grant shortcuts", () => {
  for (const key of ["S", "W"])
    assert.equal(approvalShortcut(key, { blocked: false }), null);
});
