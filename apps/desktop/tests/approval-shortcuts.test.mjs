import { test } from "node:test";
import assert from "node:assert/strict";
import { approvalShortcut } from "../src/lib/approval-shortcuts.ts";
test("an obscured, editable, repeated or non-selected approval cannot respond to a shortcut", () => {
  for (const key of ["Y", "S", "W", "N"])
    assert.equal(
      approvalShortcut(key, { blocked: true, grantable: true }),
      null,
    );
  assert.deepEqual(
    approvalShortcut("Y", { blocked: false, grantable: false }),
    { type: "once" },
  );
  assert.equal(
    approvalShortcut("W", { blocked: false, grantable: false }),
    null,
  );
});
